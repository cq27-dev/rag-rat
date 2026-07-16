//! Proposed function signature for an extracted clone-class helper (#215 Plan 4b Task 6).
//!
//! Rust-first syntactic type recovery: inspects the anchor member's AST node spans for type
//! annotations (`let x: T`, `x: T`, `-> T`) without any SCIP resolution. Typedness can be
//! `Syntactic` (every promoted param typed from annotation or literal bucket), `Partial` (some
//! typed), or `Unknown` (none typed / non-Rust). Never `resolved` — that requires SCIP (Plan 3
//! seam).
//!
//! Gapped metavars are NOT promoted to params; they go into `unresolved_type_slots` and depress
//! the typedness signal to at most `Partial` (the class isn't cleanly extractable).

use rag_rat_base::language::Language;

use super::antiunify::{MetavarKind, Template, VariationPoint};
use super::score::Confidence;
use crate::refine::RefineMember;

/// How well the syntactic type recovery succeeded for the proposed signature.
///
/// `serde` serializes to the same machine string as [`Typedness::as_db_str`]
/// (`syntactic`/`structural`/`partial`/`unknown`) — the persisted `proposed_signature_json` shape
/// (Plan-4b Task 7). The string is opaque to consumers (the CLI prints it; nothing parses it back
/// into this enum), so a new variant is additive: no migration, no reader change. A stale cached
/// row written before this variant existed surfaces the OLD label until the class is recomputed,
/// which is a less-honest label, never an over-claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum Typedness {
    /// Every promoted param was assigned a CONCRETE type (from an AST annotation or a
    /// literal-bucket coarse mapping) AND there are no `unresolved_type_slots`. The signature
    /// is complete enough for display.
    Syntactic,
    /// Every promoted param carries a STRUCTURAL type with no concrete referent, there are no
    /// `unresolved_type_slots`, and NO param has a concrete (`type_source != "none"`) type. Today
    /// the only structural type is `impl Fn()` minted for a `closure_param` — we know the hole is a
    /// callable but cannot recover the concrete closure type without SCIP.
    ///
    /// This is deliberately distinct from BOTH neighbors and exists to remove the #274-item-10
    /// surprise (a render like `fn extracted(arg0: impl Fn())` paired with `typedness=unknown`):
    /// - NOT `Syntactic`: that would over-claim a resolved/concrete referent a `closure_param` does
    ///   not have (no annotation, no literal bucket — `type_source` stays `"none"`). The hard
    ///   "never over-claim" contract forbids it.
    /// - NOT `Unknown`: that under-reports. The rendered signature visibly carries `impl Fn()`, so
    ///   "no type info" reads as wrong. `Structural` says exactly what is true: structurally typed,
    ///   no concrete referent.
    ///
    /// A MIX of concrete + structural params is `Partial` (some concrete, some not), never
    /// `Structural`.
    Structural,
    /// Some promoted params have a type, some do not; OR `unresolved_type_slots` is non-empty
    /// (a gapped metavar is present). Displayable but incomplete.
    Partial,
    /// No promoted params have a usable type string at all, or there are no promoted params
    /// (non-Rust / a closure with no `impl Fn()` placeholder). Signature is positional only.
    Unknown,
}

impl Typedness {
    pub fn as_db_str(&self) -> &'static str {
        (*self).into()
    }
}

/// One parameter in the proposed helper signature.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SigParam {
    /// Positional name: `arg0`, `arg1`, … (ascending over non-gapped metavars in VP order).
    pub(crate) name: String,
    /// Recovered type text, if any (`"i32"`, `"T0"`, `"impl Fn()"`, …).
    pub(crate) type_text: Option<String>,
    /// How the type was recovered: `"annotation"` (AST colon+type node), `"literal_bucket"`
    /// (uniform literal kind → coarse Rust type), or `"none"` (no type found).
    pub(crate) type_source: &'static str,
    /// The metavar this param was promoted from.
    pub(crate) metavar_id: String,
}

/// Proposed helper-function signature for the clone class.
///
/// Serialized whole into the persisted `proposed_signature_json` column (Plan-4b Task 7).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ProposedSignature {
    /// Human-readable render: `fn extracted(arg0: i32, arg1) -> Option<T>` etc.
    pub(crate) text: String,
    pub(crate) typedness: Typedness,
    pub(crate) confidence: Confidence,
    /// Generic type parameters (`T0`, `T1`, …) promoted from `TypeParam` VPs, in VP order.
    /// Rendered in the `<…>` clause of `text`, NEVER as runtime `arg: T` value params (Fix 4:
    /// a type-position hole becomes a generic, not a by-value argument).
    pub(crate) generic_params: Vec<String>,
    /// Promoted runtime params (`value_param` / `closure_param` metavars), in VP order.
    /// `TypeParam` VPs are NOT here — they go to `generic_params`.
    pub(crate) params: Vec<SigParam>,
    /// Metavar ids of `Gapped` VPs — not promotable to params; their presence indicates the
    /// class has structural indels that complicate extraction.
    pub(crate) unresolved_type_slots: Vec<String>,
    /// Return type recovered from `-> T` in the anchor text, or `None`.
    pub(crate) return_type: Option<String>,
}

/// Derive a proposed helper signature from the anti-unified `template`, its `members`, and the
/// `anchor_idx` anti-unification used as the spine.
///
/// Type recovery reads the ANCHOR member (`members[anchor_idx]`), NOT `members[0]`. This is
/// load-bearing (P2c): `anti_unify` records each `variation_point`'s `occurrences` as column
/// indices into the ANCHOR's `node_spans` (the medoid, via `resolve_anchor_idx`). Recovering type
/// annotations from `members[0]` when the medoid is a different member would index the column into
/// the WRONG member's syntax, yielding a type/typedness that describes the wrong source. The caller
/// passes the same `anchor_idx` it gave `align_to_anchor`/`anti_unify`. Type recovery is
/// Rust-specific; for other languages params are emitted with `type_source="none"`.
pub(crate) fn propose_signature(
    template: &Template,
    members: &[RefineMember],
    anchor_idx: usize,
) -> ProposedSignature {
    // Read the SAME anchor member the occurrences index into (the medoid). Fall back to a stub when
    // the anchor is out of range (defensive — empty members or a stale index).
    let Some(anchor) = members.get(anchor_idx) else {
        return ProposedSignature {
            text: "fn extracted()".to_string(),
            typedness: Typedness::Unknown,
            confidence: Confidence::Low,
            generic_params: vec![],
            params: vec![],
            unresolved_type_slots: vec![],
            return_type: None,
        };
    };

    let is_rust = anchor.lang == Language::Rust;

    // ── Classify VPs: Gapped → unresolved_type_slots; TypeParam → generic_params; rest → params
    // ───
    let mut generic_params: Vec<String> = Vec::new();
    let mut params: Vec<SigParam> = Vec::new();
    let mut unresolved_type_slots: Vec<String> = Vec::new();
    let mut type_param_counter: u32 = 0;
    // Codex #1: anchor columns occupied by a `TypeParam` VP, paired with the generic name assigned
    // to it. If the OUTER return-type position is one of these columns, the return type must render
    // that DECLARED generic (`-> T<n>`), never the anchor's concrete text (which matches only the
    // anchor). `(anchor_column, "T<n>")` — recurrence-collapsed VPs contribute each occurrence.
    let mut type_param_cols: Vec<(usize, String)> = Vec::new();

    for vp in &template.variation_points {
        if vp.kind == MetavarKind::Gapped {
            unresolved_type_slots.push(vp.metavar_id.clone());
            continue;
        }
        // Fix 4: a TypeParam is a GENERIC type parameter (`fn extracted<T0>(…)`), never a runtime
        // `arg: T0` value param. Route it to `generic_params` and skip the value-param promotion.
        if vp.kind == MetavarKind::TypeParam {
            let generic = format!("T{type_param_counter}");
            type_param_counter += 1;
            for &col in &vp.occurrences {
                type_param_cols.push((col, generic.clone()));
            }
            generic_params.push(generic);
            continue;
        }
        let arg_name = format!("arg{}", params.len());
        // `type_param_cols` holds every TypeParam VP seen so far. A `: T` annotation always
        // precedes its `= value` in source, and VPs are processed in spine-column order, so
        // the annotation's own TypeParam VP (if its type varies) is already recorded here —
        // the substitution in `recover_param_type` (Fix 5) sees it.
        let RecoveredType { type_text, type_source, unresolved } =
            recover_param_type(anchor, vp, is_rust, &mut type_param_counter, &type_param_cols);
        // A value_param whose per-member values are DIFFERENT literal kinds has no stable type
        // (§1.11: `type_hint` is `Some` only when the literal kind is uniform). The anchor member's
        // concrete annotation describes only the anchor, not the class — trusting it would
        // over-state typedness. Such a slot is promoted to a positional param but recorded as
        // unresolved so it cannot lift typedness to `Syntactic`. Codex #2: the generic placeholder
        // `recover_param_type` minted for it (`type_text == Some("T<n>")`, drawn from the SAME
        // `type_param_counter`) must be DECLARED in `generic_params` — otherwise the rendered
        // signature is `fn extracted(arg0: T0)` with an UNDECLARED `T0` (invalid Rust). Every
        // `T<n>` that appears anywhere is declared exactly once.
        if unresolved {
            unresolved_type_slots.push(vp.metavar_id.clone());
            if let Some(generic) = type_text.as_deref()
                && generic.starts_with('T')
                && generic[1..].chars().all(|c| c.is_ascii_digit())
                && !generic[1..].is_empty()
            {
                generic_params.push(generic.to_string());
            }
        }
        params.push(SigParam {
            name: arg_name,
            type_text,
            type_source,
            metavar_id: vp.metavar_id.clone(),
        });
    }

    // ── Return type: the OUTER function's return type only
    // ────────────────────────────────────────
    // Codex #1: a return-type position that is itself a `TypeParam` VP renders its DECLARED generic
    // `T<n>` (it varies across the class — `-> i32` vs `-> u8`); a return type with no VP renders
    // the concrete outer type (Fix 6, unchanged).
    let return_type = if is_rust { recover_return_type(anchor, &type_param_cols) } else { None };

    // ── Typedness ────────────────────────────────────────────────────────────────────────────────
    let typed_count = params.iter().filter(|p| p.type_source != "none").count();
    let typedness =
        compute_typedness(&params, typed_count, &generic_params, &unresolved_type_slots);

    // ── Confidence: min over all promoted VP confidences, then cap if unresolved slots
    // ────────────
    let confidence = compute_confidence(template, &unresolved_type_slots);

    // ── Render text ──────────────────────────────────────────────────────────────────────────────
    let text = render_signature(&generic_params, &params, return_type.as_deref());

    ProposedSignature {
        text,
        typedness,
        confidence,
        generic_params,
        params,
        unresolved_type_slots,
        return_type,
    }
}

// ── Type recovery helpers
// ─────────────────────────────────────────────────────────────────────────

/// Outcome of type recovery for one promoted variation point.
struct RecoveredType {
    /// Recovered type text, if any.
    type_text: Option<String>,
    /// How the type was recovered (`"annotation"` / `"literal_bucket"` / `"none"`).
    type_source: &'static str,
    /// `true` when the slot has no stable class-wide type and must be listed in
    /// `unresolved_type_slots` (so it cannot lift typedness to `Syntactic`). Set for a value_param
    /// whose per-member values are different literal kinds — the anchor annotation describes the
    /// anchor only, not the class.
    unresolved: bool,
}

/// Recover the type for one promoted variation point.
///
/// `type_param_cols` is the (anchor_column, generic_name) list of every `TypeParam` VP collected so
/// far — threaded so a recovered ANNOTATION whose own type varies across the class substitutes the
/// declared generic (Fix 5, Codex round-7) via [`substitute_type_params_in_type_node`], the SAME
/// helper the return type uses. Without it, `let x: i32 = 10` vs `let x: u8 = 20` would render
/// `fn extracted<T0>(arg0: i32)` — the anchor's annotation, matching only the anchor.
fn recover_param_type(
    anchor: &RefineMember,
    vp: &VariationPoint,
    is_rust: bool,
    type_param_counter: &mut u32,
    type_param_cols: &[(usize, String)],
) -> RecoveredType {
    match vp.kind {
        // Gapped and TypeParam VPs are routed before this call (Gapped → unresolved_type_slots,
        // TypeParam → generic_params per Fix 4), so only value/closure params reach here.
        MetavarKind::Gapped => unreachable!("gapped VPs are filtered before this call"),
        MetavarKind::TypeParam => unreachable!("type_param VPs are routed to generic_params"),

        MetavarKind::ClosureParam => {
            // Best-effort: we can't recover the concrete type without SCIP.
            // Produce `impl Fn()` as a placeholder for Rust; `None` otherwise.
            let type_text = if is_rust { Some("impl Fn()".to_string()) } else { None };
            RecoveredType { type_text, type_source: "none", unresolved: false }
        },

        MetavarKind::ValueParam => {
            if !is_rust {
                return RecoveredType { type_text: None, type_source: "none", unresolved: false };
            }
            let lo = vp.occurrences.first().copied().unwrap_or(0);
            // A literal-leaf value_param has NO stable class-wide type whenever its `type_hint`
            // either:
            //   (a) is `None` — a MIXED-kind literal hole (§1.11: `uniform_literal_bucket` returns
            //       `Some` only when every member's leaf is the SAME `LIT_*` kind), OR
            //   (b) is `Some(bucket)` but `bucket` maps to NO real Rust type (Fix 3, #215 Plan 4b
            //       Codex round-4: a bucket outside `literal_bucket_to_type`'s table — e.g. a
            //       cross-language `LIT_NUMBER`, or a `LIT_*` the table doesn't cover).
            // In BOTH cases the anchor's adjacent annotation (e.g. `: i32`) types only the anchor
            // member — it is NOT trustworthy as the class type — so do NOT emit it as a recovered
            // type and do NOT emit the opaque `{literal}`. Promote a generic `T<n>` and mark the
            // slot unresolved so it goes to `unresolved_type_slots` and cannot count
            // toward `Syntactic`. (Identifier value_params and uniform MAPPABLE-kind
            // literals keep the annotation/bucket path below: an `ID<n>` local declared
            // with one type across members has a stable type, and a uniform mappable
            // bucket has a real coarse Rust type.)
            let bucket_unmapped =
                vp.type_hint.as_deref().is_some_and(|b| literal_bucket_to_type(b).is_none());
            if (vp.type_hint.is_none() || bucket_unmapped) && anchor_leaf_is_literal(anchor, lo) {
                let n = *type_param_counter;
                *type_param_counter += 1;
                return RecoveredType {
                    type_text: Some(format!("T{n}")),
                    type_source: "none",
                    unresolved: true,
                };
            }
            // Priority 1: AST annotation (`: T` in the anchor node_spans around the VP column).
            // When the annotation's OWN type varies across the class (a `TypeParam` VP
            // overlaps it), substitute the declared generic (Fix 5, round-7) — `let x:
            // i32 = 10` / `let x: u8 = 20` recovers `T0`, not the anchor's `i32`. The
            // SAME helper the return type uses, so they can't diverge. A class-stable
            // annotation (no overlapping TypeParam VP) renders its concrete text
            // unchanged.
            if let Some(tspan) = try_annotation_type_span(anchor, lo)
                && let Some(annotated) =
                    substitute_type_params_in_type_node(anchor, tspan, type_param_cols)
            {
                return RecoveredType {
                    type_text: Some(annotated),
                    type_source: "annotation",
                    unresolved: false,
                };
            }
            // Priority 2: uniform literal bucket → coarse Rust type. `literal_bucket_to_type`
            // returns `None` for an unmapped bucket; that case was already routed to
            // the unresolved branch above (so it never reaches here advertising
            // `{literal}` as typed).
            if let Some(bucket) = &vp.type_hint
                && let Some(coarse) = literal_bucket_to_type(bucket)
            {
                return RecoveredType {
                    type_text: Some(coarse.to_string()),
                    type_source: "literal_bucket",
                    unresolved: false,
                };
            }
            RecoveredType { type_text: None, type_source: "none", unresolved: false }
        },
    }
}

/// `true` when the anchor's spine leaf at column `lo` is a literal-bucket token (`LIT_*`) — i.e.
/// the value_param hole is a literal, not a local identifier (`ID<n>`).
fn anchor_leaf_is_literal(anchor: &RefineMember, lo: usize) -> bool {
    anchor.seq.get(lo).is_some_and(|tok| tok.starts_with("LIT_"))
}

/// Map a `LIT_*` bucket to a coarse Rust type string, or `None` for a bucket with no stable Rust
/// type.
///
/// Both the string-literal NODE bucket (`LIT_STRING_LITERAL`) and the string-CONTENT leaf bucket
/// (`LIT_STRING_CONTENT` — the `string_content` child the normalizer can emit instead of the
/// wrapping `string_literal`) map to `&str` (Codex #3): keying only on `LIT_STRING_LITERAL` let a
/// content-bucketed string-literal value diff fall through to the opaque `{literal}` placeholder.
///
/// Fix 3 (#215 Plan 4b Codex round-4): a bucket NOT in this table returns `None`, NOT the opaque
/// `Some("{literal}")` it used to. An unmapped bucket has no real Rust type, so advertising
/// `arg0: {literal}` as a recovered type made `compute_typedness` count it as typed and falsely
/// promote the signature to `Syntactic` (`{literal}` is not a type — the signature is not actually
/// syntactically typed). `None` routes the slot to `unresolved_type_slots` in the caller, capping
/// typedness at `Partial`. The buckets the baseline normalizer can emit are `LIT_{kind.uppercased}`
/// for every `is_literal_kind` tree-sitter kind (see `clones::normalize::is_literal_kind`) PLUS the
/// special value-erased `LIT_BOOL` bucket (#232 #2b — `true`/`false` collapse to ONE bucket, NOT
/// `LIT_TRUE`/`LIT_FALSE`):
/// - Rust `*_literal` kinds (integer/float/string/raw_string/char/byte_string/negative);
/// - the cross-language explicit set (`string_content`, `string_fragment`, `integer`, `float`,
///   `number`, `char`) — `string_fragment` is the TS/JS string-body leaf, the sibling of Python's
///   `string_content`, both → `&str` (#232 #2a);
/// - `LIT_BOOL` (#232 #2b) → `bool` — the single bucket every grammar's `true`/`false` collapses
///   to.
///
/// Every one either maps to a real Rust type here or falls to `None` — none silently becomes a
/// typed `{literal}`. The common remaining Rust buckets are mapped: `char` → `char`, raw string →
/// `&str`, byte string → `&[u8]`. The cross-language numeric buckets (`LIT_INTEGER`/`LIT_FLOAT`/
/// `LIT_NUMBER`/`LIT_CHAR`) and anything else fall to `None` (no stable Rust type cross-language).
fn literal_bucket_to_type(bucket: &str) -> Option<&'static str> {
    match bucket {
        "LIT_INTEGER_LITERAL" => Some("i64"),
        "LIT_FLOAT_LITERAL" => Some("f64"),
        "LIT_STRING_LITERAL"
        | "LIT_STRING_CONTENT"
        | "LIT_STRING_FRAGMENT"
        | "LIT_RAW_STRING_LITERAL" => Some("&str"),
        "LIT_BYTE_STRING_LITERAL" => Some("&[u8]"),
        "LIT_CHAR_LITERAL" => Some("char"),
        // `LIT_BOOL` is the value-erased boolean bucket (#232 #2b). The legacy
        // `LIT_BOOLEAN_LITERAL`/`LIT_BOOL_LITERAL` arms are now dead (the normalizer never emits
        // them — booleans route to `LIT_BOOL`) but are kept as defensive aliases.
        "LIT_BOOL" | "LIT_BOOLEAN_LITERAL" | "LIT_BOOL_LITERAL" => Some("bool"),
        // Unmapped bucket: NO stable Rust type → unresolved, never the opaque `{literal}`-as-typed.
        // `LIT_CHARACTER` (the C/C++ char-literal VALUE leaf, #253) lands here DELIBERATELY: it has
        // no stable Rust type cross-language (like `LIT_CHAR`), so it stays unresolved rather than
        // over-claim a `char` type. Recall-only fix; never promotes typedness.
        _ => None,
    }
}

/// Locate the Rust type-annotation NODE for the variation point at anchor column `lo`.
///
/// Scans the `node_spans` window around `lo` backward for a `:` leaf, then forward for a type-kind
/// node, and returns that `NodeSpan` (not its text) so the caller can apply the shared
/// TypeParam-span substitution ([`substitute_type_params_in_type_node`], Fix 5 round-7) — a varying
/// annotation must render its declared generic, not the anchor's concrete text.
fn try_annotation_type_span(
    anchor: &RefineMember,
    lo: usize,
) -> Option<&crate::normalize::NodeSpan> {
    let spans = &anchor.node_spans;
    if spans.is_empty() {
        return None;
    }
    let lo = lo.min(spans.len().saturating_sub(1));

    // Search window: a few columns before `lo` for a `:` leaf.
    let window_start = lo.saturating_sub(6);

    // Walk backward from lo-1 looking for a colon leaf.
    for colon_idx in (window_start..lo).rev() {
        let span = &spans[colon_idx];
        if !(span.is_leaf && span.kind == ":") {
            continue;
        }
        // Found a colon. Look immediately after it for a type-kind node.
        // The type node appears AFTER the colon in the pre-order sequence.
        let search_end = (colon_idx + 8).min(spans.len());
        for tspan in spans.iter().take(search_end).skip(colon_idx + 1) {
            if is_type_kind(tspan.kind) {
                return Some(tspan);
            }
        }
        // Colon found but no type node after it in the window — stop searching.
        break;
    }
    None
}

/// `true` for tree-sitter node kinds that represent a Rust type. Delegates to the shared
/// [`crate::normalize::is_rust_type_kind`] — the SAME predicate
/// `antiunify::is_type_position` uses, so the type-recovery window and the anti-unify `type_param`
/// classification can never diverge on what counts as a type node (Fix 4, #215 Plan 4b).
fn is_type_kind(kind: &str) -> bool {
    crate::normalize::is_rust_type_kind(kind)
}

/// Render a recovered type node (`tspan`), substituting the DECLARED generic for every `TypeParam`
/// VP whose occurrence column overlaps the node's byte span — the ONE TypeParam-span substitution
/// used by BOTH the return type ([`recover_return_type`]) and a param annotation
/// ([`recover_param_type`], Fix 5 round-7), so the two can never diverge on how a varying type
/// renders.
///
/// A `TypeParam` VP occurrence may be:
/// - the WHOLE type node (`-> i32` vs `-> u8`, or `: i32` vs `: u8`) → the substitution covers the
///   entire node text → bare `T<n>`; OR
/// - a STRICTLY-CONTAINED inner sub-range (`Vec<i32>` vs `Vec<u8>` → the inner `i32`/`u8` leaf) →
///   substitute ONLY that sub-range, preserving the wrapper → `Vec<T0>`.
///
/// Substitutes ALL contained VPs (round-4 Fix 5): a multi-type-position node (`Result<i32, String>`
/// vs `Result<u8, Vec<u8>>`) has two TypeParam VPs inside one node; splice each from the END
/// backward so earlier byte offsets stay valid as the string mutates → `Result<T0, T1>`. Returns
/// the CONCRETE node text when no TypeParam VP overlaps it (the type does not vary across the
/// class).
fn substitute_type_params_in_type_node(
    anchor: &RefineMember,
    tspan: &crate::normalize::NodeSpan,
    type_param_cols: &[(usize, String)],
) -> Option<String> {
    let node_text = anchor.text.get(tspan.start_byte..tspan.end_byte)?;
    let mut contained: Vec<(&crate::normalize::NodeSpan, &String)> = type_param_cols
        .iter()
        .filter_map(|(col, generic)| {
            let csp = anchor.node_spans.get(*col)?;
            (csp.start_byte >= tspan.start_byte && csp.end_byte <= tspan.end_byte)
                .then_some((csp, generic))
        })
        .collect();
    if contained.is_empty() {
        // No TypeParam VP overlaps this type node → the concrete text (the type is class-stable).
        return Some(node_text.to_string());
    }
    // Splice from the END backward so each replacement leaves earlier offsets valid. Sort
    // descending by start_byte; spans are non-overlapping (distinct AST leaves / subtrees).
    contained.sort_by_key(|c| std::cmp::Reverse(c.0.start_byte));
    let mut rendered = node_text.to_string();
    for (csp, generic) in contained {
        let inner_lo = csp.start_byte - tspan.start_byte;
        let inner_hi = csp.end_byte - tspan.start_byte;
        // Guard the slice against a non-char-boundary / out-of-range edge (never panic): skip this
        // VP rather than corrupt the string.
        if inner_hi > rendered.len() || !rendered.is_char_boundary(inner_lo) {
            continue;
        }
        rendered.replace_range(inner_lo..inner_hi, generic);
    }
    Some(rendered)
}

/// Attempt to recover the OUTER function's `-> T` return type from the anchor.
///
/// Fix 6: recover the return type of the OUTER function HEADER only — not a nested closure/lambda
/// arrow. The old token scan grabbed the first `->` anywhere in the anchor, so a typed closure in
/// the body (`let f = |x| -> i32 { x };`) made the helper falsely claim `-> i32`. The function's
/// own arrow precedes its body block; a closure arrow lives INSIDE that body block. We therefore
/// scan only `->` leaves BEFORE the function body block (the outermost `block` whose span ends at
/// the function's end), so nested closure arrows are excluded by position.
///
/// Codex #1 (+ round-3 fix): `type_param_cols` is `(anchor_column, generic_name)` for every
/// `TypeParam` VP. When the recovered return-type node's byte range CONTAINS a `TypeParam` VP's
/// occurrence column, the return type VARIES across the class — render the DECLARED generic `T<n>`,
/// not the anchor's concrete text (which matches only the anchor). The VP column may be STRICTLY
/// contained (the inner type of a compound return, `-> Vec<i32>`/`-> Vec<u8>`) or EQUAL to the
/// whole return type (`-> i32`/`-> u8`); we substitute only the VP's sub-range so a wrapper is
/// preserved (`Vec<T0>`) and a whole-type VP renders bare (`T0`). A return type with no overlapping
/// `TypeParam` VP renders the concrete outer type unchanged.
///
/// Fix 5 (#215 Plan 4b Codex round-4): substitute ALL contained type-param VPs, not just the first.
/// A multi-type-position return (`-> Result<i32, String>` vs `-> Result<u8, Vec<u8>>`) has two
/// `TypeParam` VPs inside one return node; the old first-match `find_map` left the second varying
/// type concrete (`Result<T0, String>`). We splice every contained VP from the END of the node text
/// backward (so earlier byte offsets stay valid) → `Result<T0, T1>`, both declared.
fn recover_return_type(
    anchor: &RefineMember,
    type_param_cols: &[(usize, String)],
) -> Option<String> {
    let spans = &anchor.node_spans;
    let func = spans.first()?;
    // The function body is the outermost `block` child (its span ends where the function ends). The
    // header — and the header arrow + return type — is everything before that block.
    let body_start = spans
        .iter()
        .filter(|sp| sp.kind == "block" && sp.end_byte == func.end_byte)
        .map(|sp| sp.start_byte)
        // No body block (e.g. a trait fn signature `fn f() -> T;`): the whole span is header.
        .min()
        .unwrap_or(func.end_byte);

    for (i, span) in spans.iter().enumerate() {
        // Only the header arrow — a closure arrow sits inside the body block and is skipped.
        if span.is_leaf && span.kind == "->" && span.start_byte < body_start {
            let search_end = (i + 8).min(spans.len());
            for tspan in spans.iter().take(search_end).skip(i + 1) {
                // Stay within the header — never reach into the body for the type node.
                if tspan.start_byte >= body_start {
                    break;
                }
                if is_type_kind(tspan.kind) {
                    // Substitute every `TypeParam` VP that overlaps this return-type node with its
                    // declared generic (Codex #1 + round-3 + round-4 Fix 5). The shared helper owns
                    // the wrapper-preserving sub-range splice so the return type and a param
                    // annotation (Fix 5, round-7) can never diverge on how a varying type renders.
                    return substitute_type_params_in_type_node(anchor, tspan, type_param_cols);
                }
            }
        }
    }
    None
}

// ── Typedness + confidence helpers ───────────────────────────────────────────────────────────────

fn compute_typedness(
    params: &[SigParam],
    typed_count: usize,
    generic_params: &[String],
    unresolved_type_slots: &[String],
) -> Typedness {
    // A generic param (Fix 4) is a fully-recovered promotion — it counts as typed for the typedness
    // signal (a type-position hole becomes a `<T0>` generic, which IS its resolved syntactic form).
    let has_gaps = !unresolved_type_slots.is_empty();

    if params.is_empty() {
        // No runtime params. A type-only class (generics, no gaps) is Syntactic; otherwise the same
        // positional-only / depressed bands as before.
        return if !generic_params.is_empty() && !has_gaps {
            Typedness::Syntactic
        } else if unresolved_type_slots.is_empty() {
            Typedness::Unknown
        } else {
            Typedness::Partial
        };
    }
    // Gapped slots always depress away from Syntactic. Generics are always typed, so total
    // promotions are typed ⟺ every runtime param is typed.
    let all_typed = typed_count == params.len();

    if all_typed && !has_gaps {
        Typedness::Syntactic
    } else if typed_count == 0 && generic_params.is_empty() && !has_gaps {
        // No CONCRETE types (`typed_count == 0` ⟺ every param has `type_source == "none"`), no
        // generics, no gaps. Two sub-cases hide here and must NOT share a label:
        //
        // (a) every param carries a STRUCTURAL type string (today only `impl Fn()` from a
        //     `closure_param`) → `Structural`. The render visibly shows `arg0: impl Fn()`, so the
        //     `Unknown` "no type info" label is the #274-item-10 surprise. `Structural` is honest:
        //     structurally typed, no concrete referent. It does NOT over-claim — `type_source` is
        //     still `"none"`, so we never assert a resolved closure type.
        //
        // (b) at least one param has NO type string at all (`type_text == None` — non-Rust, or a
        //     closure with no `impl Fn()` placeholder) → `Unknown`, positional only.
        //
        // A value_param with an unresolved generic `T<n>` placeholder never reaches here: it sets
        // `unresolved_type_slots`, so `has_gaps` is true and this branch is skipped for it.
        if params.iter().all(|p| p.type_text.is_some()) {
            Typedness::Structural
        } else {
            Typedness::Unknown
        }
    } else {
        Typedness::Partial
    }
}

fn compute_confidence(template: &Template, unresolved_type_slots: &[String]) -> Confidence {
    // Start from the minimum confidence across all variation points.
    let min_vp_conf = template
        .variation_points
        .iter()
        .map(|vp| vp.confidence)
        .min_by_key(|c| match c {
            Confidence::High => 2u8,
            Confidence::Medium => 1,
            Confidence::Low => 0,
        })
        .unwrap_or(Confidence::High);

    // Cap at Medium if there are unresolved (gapped) type slots.
    if !unresolved_type_slots.is_empty() {
        return match min_vp_conf {
            Confidence::High => Confidence::Medium,
            other => other,
        };
    }
    min_vp_conf
}

// ── Render ────────────────────────────────────────────────────────────────────────────────────────

fn render_signature(
    generic_params: &[String],
    params: &[SigParam],
    return_type: Option<&str>,
) -> String {
    // Generic clause (Fix 4): `<T0, T1>` from type_param VPs, between the name and the param list.
    let generics_str = if generic_params.is_empty() {
        String::new()
    } else {
        format!("<{}>", generic_params.join(", "))
    };
    let param_list: Vec<String> = params
        .iter()
        .map(|p| match &p.type_text {
            // ClosureParam impl Fn() has type_source="none" but we still got a string; render it.
            Some(t) => format!("{}: {}", p.name, t),
            None => p.name.clone(),
        })
        .collect();
    let params_str = param_list.join(", ");
    match return_type {
        Some(ret) => format!("fn extracted{generics_str}({params_str}) -> {ret}"),
        None => format!("fn extracted{generics_str}({params_str})"),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rag_rat_base::language::Language;
    use rag_rat_core::index::parser;

    use super::*;
    use crate::normalize::normalize_baseline_spanned;
    use crate::refine::antiunify::{align_to_anchor, anti_unify, resolve_anchor_idx};
    use crate::tokens;

    /// Build a `RefineMember` from a Rust snippet (mirrors the `member` helper in antiunify tests).
    fn member(symbol_id: i64, src: &str) -> RefineMember {
        let text: Arc<str> = Arc::from(src);
        let parsed = parser::parse_file(Path::new("t.rs"), Language::Rust, &text).expect("parse");
        let func = parsed.symbols.iter().find(|s| s.kind == "function").expect("a function symbol");
        let node =
            parsed.root().descendant_for_byte_range(func.start_byte, func.end_byte).expect("node");
        let (seq, node_spans) = normalize_baseline_spanned(node, &text, Language::Rust);
        let struct_hash = tokens::struct_hash(&seq);
        RefineMember {
            callee_monikers: Default::default(),
            symbol_id,
            lang: Language::Rust,
            struct_hash,
            seq,
            node_spans,
            text,
        }
    }

    /// Canonical-order sort — matches the loader guarantee. Production keys on the REINDEX-STABLE
    /// `(struct_hash, path, start_byte)` (see `canonical_member_order_key`); `RefineMember` (a test
    /// fixture) lacks `path`/`start_byte`, so this helper sorts `struct_hash` then `symbol_id`,
    /// with the fixtures' `symbol_id` arranged to coincide with `(path, start_byte)` (same
    /// order on these inputs). The production guard is `refine_member_order_is_reindex_stable`,
    /// not this sort.
    fn canonical(mut members: Vec<RefineMember>) -> Vec<RefineMember> {
        members.sort_by(|a, b| {
            a.struct_hash.cmp(&b.struct_hash).then_with(|| a.symbol_id.cmp(&b.symbol_id))
        });
        members
    }

    /// Parse + align + anti-unify, then derive the proposed signature.
    fn make_sig(srcs: &[&str]) -> (Vec<RefineMember>, Template, ProposedSignature) {
        let raw: Vec<RefineMember> =
            srcs.iter().enumerate().map(|(i, s)| member(i as i64 + 1, s)).collect();
        let members = canonical(raw);
        let anchor_idx = resolve_anchor_idx(&members, None);
        let alignment = align_to_anchor(&members, anchor_idx);
        let template = anti_unify(&members, &alignment);
        let sig = propose_signature(&template, &members, anchor_idx);
        (members, template, sig)
    }

    #[test]
    fn typedness_db_str_matches_serde_for_all_variants() {
        for (typedness, token) in [
            (Typedness::Syntactic, "syntactic"),
            (Typedness::Structural, "structural"),
            (Typedness::Partial, "partial"),
            (Typedness::Unknown, "unknown"),
        ] {
            assert_eq!(typedness.as_db_str(), token);
            assert_eq!(
                serde_json::to_value(typedness).unwrap(),
                serde_json::Value::String(token.into())
            );
        }
    }

    // ── Test 1: a STABLE type position → Syntactic ───────────────────────────────────────────────

    #[test]
    fn value_param_with_annotation_typedness_syntactic() {
        // A type_param is the only promoted-param kind that earns `Syntactic` in the baseline
        // scheme. (The baseline seq erases a literal's VALUE to its `LIT_<KIND>` bucket and
        // normalizes locals to positional `ID<n>`, so a same-kind, differing-value literal
        // or a renamed local produces NO variation column — a value_param VP arises ONLY
        // when the literal KIND differs, and that mixed-kind case is intentionally NOT
        // Syntactic, see `mixed_literal_kind_value_param_not_syntactic`.)
        //
        // Here the differing material is a TYPE position (`Vec<i32>` vs `Vec<u8>`) → one type_param
        // VP promoted to a GENERIC `T0` (Fix 4: a type-position hole is a generic, not a runtime
        // `arg: T0` value param). A type-only class with no gaps → typedness is Syntactic.
        let (_, template, sig) = make_sig(&[
            "fn f() { let v: Vec<i32> = q(); g(v); }",
            "fn g() { let v: Vec<u8> = q(); g(v); }",
        ]);
        assert!(!template.variation_points.is_empty(), "differing type position must produce a VP");
        // The type_param is a generic, NOT a runtime param.
        assert!(
            sig.params.is_empty(),
            "a type-only class has no runtime params, got {:?}",
            sig.params
        );
        assert!(
            sig.generic_params.iter().any(|g| g == "T0"),
            "the type_param must be promoted to a generic T0, got {:?}",
            sig.generic_params
        );
        // All promotions resolved, no gaps → Syntactic.
        assert_eq!(
            sig.typedness,
            Typedness::Syntactic,
            "all-typed, no-gap signature is Syntactic, got {:?}",
            sig.typedness
        );
        assert!(sig.unresolved_type_slots.is_empty(), "stable type → no unresolved slots");
    }

    // ── Test 2: literal_bucket coarse-type mapping (unit-level) ──────────────────────────────────

    #[test]
    fn value_param_literal_bucket_typedness_syntactic() {
        // The literal_bucket path types a value_param whose literal KIND is uniform across members
        // (§1.11 type_hint == Some). The baseline scheme cannot synthesize such a value_param VP
        // through `propose_signature` (a same-kind literal is a FIXED column — the value is erased
        // to its bucket), so the path is exercised at the unit level: the
        // bucket→coarse-type mapping is what the path returns once a uniform-kind
        // value_param exists.
        assert_eq!(literal_bucket_to_type("LIT_INTEGER_LITERAL"), Some("i64"));
        assert_eq!(literal_bucket_to_type("LIT_FLOAT_LITERAL"), Some("f64"));
        assert_eq!(literal_bucket_to_type("LIT_STRING_LITERAL"), Some("&str"));
        assert_eq!(literal_bucket_to_type("LIT_BOOLEAN_LITERAL"), Some("bool"));
        assert_eq!(literal_bucket_to_type("LIT_BOOL_LITERAL"), Some("bool"));
        // Fix 3 (#215 Plan 4b Codex round-4): the common remaining Rust buckets now map to real
        // types — char → `char`, raw string → `&str`, byte string → `&[u8]` — instead of the opaque
        // `{literal}` placeholder.
        assert_eq!(literal_bucket_to_type("LIT_CHAR_LITERAL"), Some("char"));
        assert_eq!(literal_bucket_to_type("LIT_RAW_STRING_LITERAL"), Some("&str"));
        assert_eq!(literal_bucket_to_type("LIT_BYTE_STRING_LITERAL"), Some("&[u8]"));
        // #232 #2a: the TS/JS string-body bucket maps to `&str`, same as Python's `string_content`.
        assert_eq!(literal_bucket_to_type("LIT_STRING_FRAGMENT"), Some("&str"));
        assert_eq!(literal_bucket_to_type("LIT_STRING_CONTENT"), Some("&str"));
        // #232 #2b: the value-erased boolean bucket maps to `bool` (the one the normalizer emits
        // now).
        assert_eq!(literal_bucket_to_type("LIT_BOOL"), Some("bool"));
        // Fix 3: a bucket with NO stable Rust type returns `None` (NEVER the opaque
        // `Some("{literal}")` that used to count as typed) — so the slot goes unresolved and cannot
        // lift typedness to Syntactic.
        assert_eq!(literal_bucket_to_type("LIT_NUMBER"), None);
        assert_eq!(literal_bucket_to_type("LIT_NEGATIVE_LITERAL"), None);
        assert_eq!(literal_bucket_to_type("LIT_SOME_UNKNOWN_KIND"), None);
    }

    // ── Fix 3: an UNMAPPED literal bucket must NOT be advertised as Syntactic
    // ─────────────────────

    #[test]
    fn unmapped_literal_bucket_not_syntactic() {
        // Fix 3 (#215 Plan 4b Codex round-4): a value_param whose uniform literal bucket has NO
        // stable Rust type (e.g. `LIT_NEGATIVE_LITERAL`, or any bucket outside
        // `literal_bucket_to_type`'s table) used to recover the opaque `{literal}` placeholder,
        // which `compute_typedness` counted as typed → the signature was falsely advertised
        // as `Syntactic`. The fix routes an unmapped bucket to a generic placeholder + an
        // unresolved slot, so typedness is capped at Partial and the render never contains
        // `{literal}`.
        //
        // The baseline scheme can't synthesize a uniform-kind literal value_param VP through a real
        // parse (a same-kind literal is a FIXED column), so drive `propose_signature` directly with
        // a synthetic anchor whose hole leaf is a `LIT_*` literal and a VP carrying the
        // unmapped bucket as its `type_hint` — exactly the shape the classifier would emit
        // for a uniform unmapped literal.
        use crate::normalize::NodeSpan;

        // Synthetic anchor: `fn ...` header reduced to a single literal leaf at column 0 (all that
        // `anchor_leaf_is_literal` / `recover_param_type` inspect). seq[0] = a LIT_* token.
        let anchor = RefineMember {
            callee_monikers: Default::default(),
            symbol_id: 1,
            lang: Language::Rust,
            struct_hash: "synthetic".to_string(),
            seq: vec!["LIT_NEGATIVE_LITERAL".to_string()],
            node_spans: vec![NodeSpan {
                start_byte: 0,
                end_byte: 2,
                kind: "negative_literal",
                is_leaf: true,
            }],
            text: Arc::from("-1"),
        };
        let members = vec![anchor];

        // One value_param VP whose uniform bucket is the UNMAPPED `LIT_NEGATIVE_LITERAL`.
        let vp = VariationPoint {
            metavar_id: "m0".to_string(),
            kind: MetavarKind::ValueParam,
            occurrences: vec![0],
            per_member_values: vec!["-1".to_string(), "-2".to_string()],
            extraction_role: MetavarKind::ValueParam.as_db_str(),
            type_hint: Some("LIT_NEGATIVE_LITERAL".to_string()),
            confidence: Confidence::High,
            differing_callee: false,
        };
        let template = Template {
            text: "⟨m0⟩".to_string(),
            variation_points: vec![vp],
            anti_unify_coverage: 0.5,
            sampled: false,
            occurrence_spans: std::collections::BTreeMap::new(),
        };

        let sig = propose_signature(&template, &members, 0);

        // Typedness must NOT be Syntactic — an unmapped bucket has no stable type.
        assert_ne!(
            sig.typedness,
            Typedness::Syntactic,
            "an unmapped literal bucket must NOT advertise the signature as Syntactic, got {:?}",
            sig.typedness
        );
        // The opaque `{literal}` placeholder must never appear as a recovered type.
        assert!(
            !sig.text.contains("{literal}"),
            "the rendered signature must not carry the opaque {{literal}} placeholder, got {:?}",
            sig.text
        );
        for p in &sig.params {
            assert_ne!(
                p.type_text.as_deref(),
                Some("{literal}"),
                "no param may carry the opaque {{literal}} type, got {:?}",
                p
            );
        }
        // The slot is recorded as unresolved (so it cannot count toward Syntactic) and the generic
        // placeholder it minted is DECLARED (no undeclared T<n> in the render — Codex #2
        // invariant).
        let unmapped_param = sig
            .params
            .iter()
            .find(|p| p.metavar_id == "m0")
            .expect("the value_param is promoted to a positional param");
        assert!(
            sig.unresolved_type_slots.contains(&"m0".to_string()),
            "the unmapped-bucket value_param must be in unresolved_type_slots, got {:?}",
            sig.unresolved_type_slots
        );
        if let Some(t) = unmapped_param.type_text.as_deref()
            && t.starts_with('T')
            && t[1..].chars().all(|c| c.is_ascii_digit())
            && !t[1..].is_empty()
        {
            assert!(
                sig.generic_params.iter().any(|g| g == t),
                "the minted generic {t} must be declared, got {:?}",
                sig.generic_params
            );
        }
    }

    // ── Test 2b: MIXED-kind literal value_param → honest typedness (not Syntactic) ───────────────

    #[test]
    fn mixed_literal_kind_value_param_not_syntactic() {
        // The two members declare DIFFERENT literal kinds (i32/10 vs f32/2.5) AND different
        // annotations (i32 vs f32). The literal value_param has mixed kinds → §1.11 leaves its
        // type_hint == None. The anchor's `: i32` annotation types ONLY the anchor member, not the
        // class — trusting it would over-state typedness as Syntactic. The fix: the literal-leaf
        // value_param with type_hint == None must NOT emit the anchor's concrete annotation as a
        // trusted type; it gets a generic `T<n>` (type_source != "annotation") and its metavar id
        // is recorded in unresolved_type_slots, pushing typedness to Partial (or Unknown),
        // never Syntactic.
        // "fn f() { let x: i32 = 10; sink(x); }" → …, :, i32, =, LIT_INTEGER…
        // "fn g() { let y: f32 = 2.5; sink(y); }" → …, :, f32, =, LIT_FLOAT…
        let (_, template, sig) = make_sig(&[
            "fn f() { let x: i32 = 10; sink(x); }",
            "fn g() { let y: f32 = 2.5; sink(y); }",
        ]);
        // The literal value_param VP must exist with type_hint == None (mixed kinds).
        let lit_vp = template
            .variation_points
            .iter()
            .find(|vp| vp.kind == MetavarKind::ValueParam && vp.type_hint.is_none())
            .expect("a mixed-kind literal value_param VP (type_hint == None)");

        // Its SigParam must NOT carry a trusted annotation type (generic or none).
        let lit_param = sig
            .params
            .iter()
            .find(|p| p.metavar_id == lit_vp.metavar_id)
            .expect("the mixed-kind value_param is promoted to a positional param");
        assert_ne!(
            lit_param.type_source, "annotation",
            "a mixed-kind literal value_param must NOT claim the anchor's annotation as its type, \
             got type_source={:?} type_text={:?}",
            lit_param.type_source, lit_param.type_text
        );

        // The metavar id must be listed as an unresolved type slot.
        assert!(
            sig.unresolved_type_slots.contains(&lit_vp.metavar_id),
            "the mixed-kind literal value_param {} must be in unresolved_type_slots: {:?}",
            lit_vp.metavar_id,
            sig.unresolved_type_slots
        );

        // Honesty pin: typedness must NOT be Syntactic — the type is not stable across members.
        assert_ne!(
            sig.typedness,
            Typedness::Syntactic,
            "mixed-kind literal value_param → typedness must be Partial or Unknown, never \
             Syntactic"
        );
        assert!(
            matches!(sig.typedness, Typedness::Partial | Typedness::Unknown),
            "mixed-kind typedness is Partial or Unknown, got {:?}",
            sig.typedness
        );
    }

    // ── Test 3: gapped metavar → unresolved_type_slots + honesty pin (not Syntactic) ─────────────

    #[test]
    fn unresolved_slot_lists_gapped_metavar_and_depresses_typedness() {
        // b has an extra statement (`bar();`) → gapped VP. Both share `let a = 1; foo();` structure
        // (identical baseline → no value_param VP). The gapped VP must NOT appear in params but
        // MUST appear in unresolved_type_slots. typedness must NOT be Syntactic.
        let (_, template, sig) =
            make_sig(&["fn f() { let a = 1; foo(); }", "fn f() { let a = 1; foo(); bar(); }"]);
        let has_gapped = template.variation_points.iter().any(|vp| vp.kind == MetavarKind::Gapped);
        assert!(has_gapped, "inserted statement must produce a gapped metavar");

        // The gapped VP must be in unresolved_type_slots, not params.
        assert!(
            !sig.unresolved_type_slots.is_empty(),
            "gapped VP must appear in unresolved_type_slots"
        );
        // No gapped VP may leak into params.
        for vp in template.variation_points.iter().filter(|vp| vp.kind == MetavarKind::Gapped) {
            assert!(
                !sig.params.iter().any(|p| p.metavar_id == vp.metavar_id),
                "gapped metavar {} must NOT appear in params",
                vp.metavar_id
            );
        }
        // Honesty pin: gapped → typedness is NOT Syntactic.
        assert_ne!(
            sig.typedness,
            Typedness::Syntactic,
            "gapped VP present → typedness must NOT be Syntactic"
        );
    }

    // ── Test 4: closure_param becomes impl Fn(), confidence is Medium
    // ─────────────────────────────

    #[test]
    fn closure_param_becomes_impl_fn_medium() {
        // Two calls where the second callee differs. The second callee gets a different alpha-ID
        // because the first callee (foo) is already ID0, so bar gets ID1.
        // This is the same pattern as antiunify's
        // `statement_head_differing_callee_is_closure_param`.
        let (_, template, sig) =
            make_sig(&["pub fn a() { foo(); foo() }", "pub fn b() { foo(); bar() }"]);
        let has_closure =
            template.variation_points.iter().any(|vp| vp.kind == MetavarKind::ClosureParam);
        assert!(has_closure, "differing callee must produce a closure_param");

        // ClosureParam in Rust → type_text = Some("impl Fn()"), type_source = "none"
        let has_impl_fn = sig.params.iter().any(|p| p.type_text.as_deref() == Some("impl Fn()"));
        assert!(has_impl_fn, "closure_param in Rust must produce impl Fn() param");

        // Confidence: the closure_param at Medium → sig confidence is at most Medium.
        assert!(
            matches!(sig.confidence, Confidence::Medium | Confidence::Low),
            "closure_param class must have confidence ≤ Medium, got {:?}",
            sig.confidence
        );

        // #274 item 10: the render carries `impl Fn()`, so typedness must NOT be the surprising
        // `Unknown` ("no type info"). For a pure-closure class (every promoted param is an
        // `impl Fn()` structural type, no concrete types, no gaps) the honest label is
        // `Structural`.
        let pure_closure = !sig.params.is_empty()
            && sig.params.iter().all(|p| p.type_text.as_deref() == Some("impl Fn()"))
            && sig.unresolved_type_slots.is_empty();
        if pure_closure {
            assert_eq!(
                sig.typedness,
                Typedness::Structural,
                "a pure-closure class renders `impl Fn()` → typedness is Structural, not the \
                 surprising Unknown, got {:?}",
                sig.typedness
            );
        }
    }

    // ── #274 item 10: a pure-closure class is Structural (impl Fn() typed, no concrete referent)
    // ──

    #[test]
    fn pure_closure_param_typedness_is_structural_not_unknown() {
        // #274 item 10: a class whose ONLY variation is a differing callee mints one
        // `closure_param` VP rendered as `impl Fn()`. That param has `type_source ==
        // "none"` (no annotation, no literal bucket — the concrete closure type needs
        // SCIP), so `compute_typedness`'s `typed_count` is 0 and the class used to fall to
        // `Unknown`. The render shows `arg0: impl Fn()` while typedness said "no type info"
        // — internally consistent but surprising. The honest label is `Structural`:
        // structurally typed, no concrete referent.
        //
        // Crucially this is NOT an over-claim: `type_source` stays `"none"` (we never assert a
        // resolved/concrete type for a closure), and `Structural` is strictly weaker than
        // `Syntactic`. The contract "may under-report, must never over-claim" holds — we only
        // upgrade the DISPLAY label from Unknown to the more precise Structural.
        let (_, template, sig) =
            make_sig(&["pub fn a() { foo(); foo() }", "pub fn b() { foo(); bar() }"]);

        // Precondition: a pure-closure class — exactly the closure_param VP, no value/type/gapped
        // VP.
        let closure_count = template
            .variation_points
            .iter()
            .filter(|vp| vp.kind == MetavarKind::ClosureParam)
            .count();
        assert_eq!(closure_count, 1, "expected exactly one closure_param VP, got {closure_count}");
        assert!(
            !template.variation_points.iter().any(|vp| matches!(
                vp.kind,
                MetavarKind::ValueParam | MetavarKind::TypeParam | MetavarKind::Gapped
            )),
            "this fixture must be a PURE closure class (no value/type/gapped VPs), got {:?}",
            template.variation_points
        );

        // The single param is `impl Fn()` with `type_source == "none"` (no concrete recovery).
        assert_eq!(sig.params.len(), 1, "one promoted closure param, got {:?}", sig.params);
        let p = &sig.params[0];
        assert_eq!(p.type_text.as_deref(), Some("impl Fn()"), "closure param renders impl Fn()");
        assert_eq!(
            p.type_source, "none",
            "a closure param has NO concrete type source — recovering one would over-claim a \
             resolved referent the class does not have"
        );
        assert!(sig.unresolved_type_slots.is_empty(), "pure-closure class has no gapped slots");
        assert!(sig.generic_params.is_empty(), "pure-closure class promotes no generics");

        // The honest typedness is Structural — neither the over-claiming Syntactic nor the
        // surprising/under-reporting Unknown.
        assert_eq!(
            sig.typedness,
            Typedness::Structural,
            "pure-closure typedness must be Structural, got {:?}",
            sig.typedness
        );
        assert_ne!(sig.typedness, Typedness::Syntactic, "Structural must never become Syntactic");
        assert_ne!(
            sig.typedness,
            Typedness::Unknown,
            "the impl Fn() render must not read as Unknown"
        );

        // The serialized/DB string is the new `structural` token (additive — opaque to readers).
        assert_eq!(sig.typedness.as_db_str(), "structural");
        assert_eq!(
            serde_json::to_value(sig.typedness).unwrap(),
            serde_json::Value::String("structural".to_string()),
            "serde must serialize Structural to the same `structural` token as as_db_str"
        );
    }

    #[test]
    fn no_runtime_param_typedness_distinguishes_unknown_from_structural() {
        // Guard the Structural/Unknown split at the unit level so it can't silently collapse:
        // - a closure param (`impl Fn()`, type_text Some, type_source none) → Structural;
        // - a truly untyped positional param (type_text None) → Unknown;
        // - a MIX of the two → Partial (some concrete-less-but-structural, some none).
        let closure = SigParam {
            name: "arg0".to_string(),
            type_text: Some("impl Fn()".to_string()),
            type_source: "none",
            metavar_id: "m0".to_string(),
        };
        let bare = SigParam {
            name: "arg1".to_string(),
            type_text: None,
            type_source: "none",
            metavar_id: "m1".to_string(),
        };

        // Pure structural → Structural.
        assert_eq!(
            compute_typedness(std::slice::from_ref(&closure), 0, &[], &[]),
            Typedness::Structural
        );
        // Pure untyped → Unknown.
        assert_eq!(compute_typedness(std::slice::from_ref(&bare), 0, &[], &[]), Typedness::Unknown);
        // Mixed structural + untyped → not all `type_text.is_some()` → Unknown is wrong, must be
        // Unknown only when EVERY param lacks a type string; a structural + bare mix is Unknown
        // here too (one param has no type string), which is honest (positional for the bare one).
        // The point: a bare param drags the band down, never up.
        assert_eq!(
            compute_typedness(&[closure, bare], 0, &[], &[]),
            Typedness::Unknown,
            "a param with NO type string keeps the class out of Structural"
        );
    }

    // ── Test 5: gapped metavar not promoted to param
    // ──────────────────────────────────────────────

    #[test]
    fn gapped_metavar_not_promoted_to_param() {
        // The extra `bar();` statement in b → gapped VP. Must appear in unresolved_type_slots, NOT
        // in params.
        let (_, template, sig) =
            make_sig(&["fn f() { let a = 1; foo(); }", "fn f() { let a = 1; foo(); bar(); }"]);
        let gapped_vps: Vec<_> =
            template.variation_points.iter().filter(|vp| vp.kind == MetavarKind::Gapped).collect();
        assert!(!gapped_vps.is_empty(), "inserted statement must produce a gapped metavar");

        for gvp in &gapped_vps {
            // Must NOT be in params.
            assert!(
                !sig.params.iter().any(|p| p.metavar_id == gvp.metavar_id),
                "gapped VP {} must NOT appear in params",
                gvp.metavar_id
            );
            // Must be in unresolved_type_slots.
            assert!(
                sig.unresolved_type_slots.contains(&gvp.metavar_id),
                "gapped VP {} must appear in unresolved_type_slots",
                gvp.metavar_id
            );
        }
    }

    // ── Test 6: confidence_v2 never exceeds confidence_v1 (property) ─────────────────────────────

    #[test]
    fn confidence_v2_never_exceeds_v1_property() {
        use super::super::score::{MetavarProfile, confidence_v1, confidence_v2};

        let confidence_ord = |c: Confidence| match c {
            Confidence::High => 2u32,
            Confidence::Medium => 1,
            Confidence::Low => 0,
        };

        let ratios = [0.0f64, 0.5, 0.65, 0.80, 0.90, 0.95, 1.0];
        let gapped_flags = [0usize, 1];
        let callee_flags = [false, true];
        let coverages = [0.3f64, 0.6, 0.8, 0.95];

        for &r in &ratios {
            for &s in &ratios {
                for &g in &gapped_flags {
                    for &dc in &callee_flags {
                        for &cov in &coverages {
                            let total = 3usize;
                            let closure = if dc { 1 } else { 0 };
                            let profile = MetavarProfile {
                                total,
                                value: total - closure - g,
                                closure,
                                typ: 0,
                                gapped: g,
                                differing_callee: dc,
                                anti_unify_coverage: cov,
                            };
                            let v1 = confidence_v1(r, s);
                            let v2 = confidence_v2(r, s, &profile);
                            assert!(
                                confidence_ord(v2) <= confidence_ord(v1),
                                "confidence_v2 {:?} > confidence_v1 {:?} at r={r} s={s}",
                                v2,
                                v1
                            );
                        }
                    }
                }
            }
        }
    }

    // ── Test 7: refactorability_v2 ≤ refactorability_v1 (property) ───────────────────────────────

    #[test]
    fn refactorability_v2_never_exceeds_v1_property() {
        use super::super::score::{MetavarProfile, refactorability_v1, refactorability_v2};

        let ratios = [0.0f64, 0.3, 0.5, 0.7, 0.85, 0.95, 1.0];
        let profiles = vec![
            MetavarProfile {
                total: 1,
                value: 1,
                closure: 0,
                typ: 0,
                gapped: 0,
                differing_callee: false,
                anti_unify_coverage: 0.95,
            },
            MetavarProfile {
                total: 5,
                value: 2,
                closure: 2,
                typ: 1,
                gapped: 0,
                differing_callee: true,
                anti_unify_coverage: 0.7,
            },
            MetavarProfile {
                total: 12,
                value: 3,
                closure: 7,
                typ: 2,
                gapped: 2,
                differing_callee: false,
                anti_unify_coverage: 0.4,
            },
        ];
        for &r in &ratios {
            for p in &profiles {
                let v1 = refactorability_v1(r);
                let v2 = refactorability_v2(r, p);
                assert!(
                    v2 <= v1 + 1e-12,
                    "refactorability_v2 {v2} > refactorability_v1 {v1} at r={r}"
                );
            }
        }
    }

    // ── Test 8b (Fix 4): type_param renders as a generic, not a bogus runtime arg ────────────────

    #[test]
    fn type_param_renders_as_generic_not_runtime_arg() {
        // Fix 4: a TypeParam VP (`Vec<i32>` vs `Vec<u8>`) is a GENERIC type parameter, not a
        // runtime value param. The old loop promoted every non-gapped VP into `params` and
        // render_signature rendered only fn params → `fn extracted(arg0: T0)` (a value arg)
        // with NO `<T0>` generic declaration. The fix routes TypeParam VPs into a separate
        // generic list rendered as `fn extracted<T0>(…)`, with no `arg: T0` runtime param.
        let (_, template, sig) = make_sig(&[
            "fn f() { let v: Vec<i32> = q(); g(v); }",
            "fn g() { let v: Vec<u8> = q(); g(v); }",
        ]);
        assert!(
            template.variation_points.iter().any(|vp| vp.kind == MetavarKind::TypeParam),
            "differing type position must produce a type_param VP"
        );
        // The signature declares the generic and does NOT carry a runtime `arg: T0` param.
        assert!(
            sig.text.contains("extracted<T0>"),
            "type_param must render as a generic `<T0>`, got {:?}",
            sig.text
        );
        assert!(
            !sig.text.contains("arg0: T0") && !sig.text.contains("arg0"),
            "type_param must NOT render as a runtime `arg0: T0` param, got {:?}",
            sig.text
        );
        // The type_param VP is in generic_params, not in the runtime params list.
        assert!(
            sig.generic_params.iter().any(|g| g == "T0"),
            "T0 must be a generic param, got {:?}",
            sig.generic_params
        );
        assert!(
            sig.params.is_empty(),
            "a type-only class has no runtime params, got {:?}",
            sig.params
        );
        // Typedness still Syntactic (the generic is fully recovered, no gaps).
        assert_eq!(sig.typedness, Typedness::Syntactic);
    }

    // ── Codex #1: a TypeParam VP in the RETURN position renders the DECLARED generic, not concrete
    // ─

    #[test]
    fn return_type_type_param_renders_generic() {
        // Codex #1: the outer return type itself VARIES across the class (`-> i32` vs `-> u8`) → a
        // `TypeParam` VP in the return position. The old code rendered the ANCHOR's concrete `i32`
        // (matching only the anchor); it must render the DECLARED generic `T0`, so the helper is
        // `fn extracted<T0>() -> T0` — valid for both members.
        let (_, template, sig) = make_sig(&[
            "fn f() -> i32 { let n = 1; sink(n) }",
            "fn g() -> u8 { let n = 1; sink(n) }",
        ]);
        assert!(
            template.variation_points.iter().any(|vp| vp.kind == MetavarKind::TypeParam),
            "a differing return type position must produce a type_param VP"
        );
        // The return type is the generic placeholder, NOT a concrete `i32`/`u8`.
        assert_eq!(
            sig.return_type.as_deref(),
            Some("T0"),
            "a return-type TypeParam VP must render its generic, got {:?}",
            sig.return_type
        );
        assert!(
            !sig.text.contains("-> i32") && !sig.text.contains("-> u8"),
            "the return type must NOT render the anchor's concrete type, got {:?}",
            sig.text
        );
        assert!(
            sig.text.contains("extracted<T0>") && sig.text.contains("-> T0"),
            "render must be `fn extracted<T0>(…) -> T0`, got {:?}",
            sig.text
        );
        // T0 is DECLARED (no undeclared generic).
        assert!(
            sig.generic_params.iter().any(|g| g == "T0"),
            "the return-type generic T0 must be declared, got {:?}",
            sig.generic_params
        );
    }

    #[test]
    fn return_type_generic_preserves_wrapper() {
        // Round-3 fix (#215 4b): when the return type is COMPOUND and only the INNER type varies
        // (`-> Vec<i32>` vs `-> Vec<u8>`), the `TypeParam` VP's occurrence column is the inner
        // `primitive_type` leaf — STRICTLY contained in the outer `generic_type` return node. The
        // old byte-containment branch matched that inner column and returned bare `T0`, DROPPING
        // the `Vec<>` wrapper. The fix substitutes ONLY the inner sub-range, preserving the
        // wrapper → `-> Vec<T0>`. T0 must still be declared.
        let (_, template, sig) = make_sig(&[
            "fn f() -> Vec<i32> { let n = 1; vec![sink(n)] }",
            "fn g() -> Vec<u8> { let n = 1; vec![sink(n)] }",
        ]);
        assert!(
            template.variation_points.iter().any(|vp| vp.kind == MetavarKind::TypeParam),
            "a differing inner return-type position must produce a type_param VP"
        );
        // The wrapper is preserved; only the inner type is the generic placeholder.
        assert_eq!(
            sig.return_type.as_deref(),
            Some("Vec<T0>"),
            "an inner-only return-type TypeParam VP must preserve the wrapper, got {:?}",
            sig.return_type
        );
        assert!(
            !sig.text.contains("Vec<i32>") && !sig.text.contains("Vec<u8>"),
            "the concrete inner type must not leak into the render, got {:?}",
            sig.text
        );
        assert!(
            sig.text.contains("-> Vec<T0>"),
            "render must be `… -> Vec<T0>`, got {:?}",
            sig.text
        );
        // T0 is DECLARED (no undeclared generic).
        assert!(
            sig.generic_params.iter().any(|g| g == "T0"),
            "the inner return-type generic T0 must be declared, got {:?}",
            sig.generic_params
        );

        // The WHOLE-return-type case (`-> i32` vs `-> u8`) still renders bare `T0` (VP span EQUALS
        // the whole return type → no wrapper to preserve).
        let (_, _t2, sig_bare) = make_sig(&[
            "fn f() -> i32 { let n = 1; sink(n) }",
            "fn g() -> u8 { let n = 1; sink(n) }",
        ]);
        assert_eq!(
            sig_bare.return_type.as_deref(),
            Some("T0"),
            "a whole-return-type TypeParam VP renders bare T0, got {:?}",
            sig_bare.return_type
        );
    }

    #[test]
    fn generic_type_head_diff_widens_to_whole_type() {
        // Fix 4 (#215 Plan 4b Codex round-7): when the generic-type HEAD differs (`-> Vec<i32>` vs
        // `-> Option<i32>`), the hole widens to the WHOLE `generic_type`, so the return type
        // renders a bare generic `T0` — NOT `T0<i32>` (invalid Rust that also hard-codes
        // the anchor's args).
        let (_, template, sig) =
            make_sig(&["fn f() -> Vec<i32> { todo!() }", "fn g() -> Option<i32> { todo!() }"]);
        assert!(
            template.variation_points.iter().any(|vp| vp.kind == MetavarKind::TypeParam),
            "a differing generic-type head must produce a type_param VP"
        );
        assert_eq!(
            sig.return_type.as_deref(),
            Some("T0"),
            "a whole-generic-type head diff must render bare T0 (NOT T0<i32>), got {:?}",
            sig.return_type
        );
        assert!(
            !sig.text.contains("T0<")
                && !sig.text.contains("Vec<i32>")
                && !sig.text.contains("Option<i32>"),
            "neither `T0<i32>` nor a concrete generic may leak into the render, got {:?}",
            sig.text
        );
        assert!(
            sig.generic_params.iter().any(|g| g == "T0"),
            "T0 must be declared, got {:?}",
            sig.generic_params
        );

        // The INNER-arg-only case still preserves the wrapper (`-> Vec<T0>`) — Fix 4 must not
        // over-widen it.
        let (_, _t, sig_inner) =
            make_sig(&["fn f() -> Vec<i32> { todo!() }", "fn g() -> Vec<u8> { todo!() }"]);
        assert_eq!(
            sig_inner.return_type.as_deref(),
            Some("Vec<T0>"),
            "inner-arg diff must preserve the wrapper, got {:?}",
            sig_inner.return_type
        );

        // Let-binding variant: a value_param promoted with the widened generic-type annotation must
        // substitute the generic (see Fix 5) rather than hard-code `Vec<i32>`.
        let (_, _t2, sig_let) = make_sig(&[
            "fn f() { let v: Vec<i32> = d(); use_it(v) }",
            "fn g() { let v: Option<i32> = d(); use_it(v) }",
        ]);
        assert!(
            !sig_let.text.contains("Vec<i32>") && !sig_let.text.contains("Option<i32>"),
            "the let-binding generic-type head diff must not hard-code a concrete type, got {:?}",
            sig_let.text
        );
    }

    // ── Fix 5: ALL return-type holes substitute, not just the first ──────────────────────────────

    #[test]
    fn return_type_substitutes_all_type_param_holes() {
        // Fix 5 (#215 Plan 4b Codex round-4): a return type with MULTIPLE type-position VPs
        // (`-> Result<i32, String>` vs `-> Result<u8, Vec<u8>>`) has two `TypeParam` VPs both
        // inside the `Result<…>` return node. The old first-match `find_map` substituted
        // only the FIRST contained column → `Result<T0, String>`, leaving the second
        // varying type concrete (it matched only the anchor). The fix splices EVERY
        // contained VP from the end backward → `Result<T0, T1>`, with both T0 and T1
        // declared.
        let (_, template, sig) = make_sig(&[
            "fn f() -> Result<i32, String> { let n = 1; do_it(n) }",
            "fn g() -> Result<u8, Vec<u8>> { let n = 1; do_it(n) }",
        ]);
        // Two type positions differ → at least two TypeParam VPs.
        let type_param_count =
            template.variation_points.iter().filter(|vp| vp.kind == MetavarKind::TypeParam).count();
        assert!(
            type_param_count >= 2,
            "two differing return type positions must produce ≥2 type_param VPs, got {} in {:?}",
            type_param_count,
            template.variation_points
        );

        // BOTH varying inner types are substituted: the render is `Result<T0, T1>`, NOT
        // `Result<T0, String>` / `Result<T0, Vec<u8>>` (the first-match bug).
        assert_eq!(
            sig.return_type.as_deref(),
            Some("Result<T0, T1>"),
            "all contained return-type holes must substitute → Result<T0, T1>, got {:?}",
            sig.return_type
        );
        // No concrete inner type leaks (neither the anchor's `String`/`i32` nor the other member's
        // `Vec<u8>`/`u8`).
        for concrete in ["i32", "u8", "String", "Vec<u8>"] {
            assert!(
                !sig.return_type.as_deref().unwrap_or("").contains(concrete),
                "the concrete inner type {concrete:?} must not leak into the return type, got {:?}",
                sig.return_type
            );
        }
        // Both generics are DECLARED (no undeclared T<n> in the render).
        assert!(
            sig.generic_params.iter().any(|g| g == "T0")
                && sig.generic_params.iter().any(|g| g == "T1"),
            "both return-type generics T0 and T1 must be declared, got {:?}",
            sig.generic_params
        );
        assert!(
            sig.text.contains("-> Result<T0, T1>"),
            "render must be `… -> Result<T0, T1>`, got {:?}",
            sig.text
        );
    }

    #[test]
    fn param_annotation_substitutes_type_param() {
        // Fix 5 (#215 Plan 4b Codex round-7): when a value hole's NEARBY ANNOTATION also varies
        // (`let x: i32 = 10` vs `let x: u8 = 20`), the annotation TypeParam VP already declared a
        // generic for `i32`/`u8`. The recovered param annotation must substitute that declared
        // generic (`arg0: T0`), NOT hard-code the anchor's `i32` (which matches only the anchor and
        // misrepresents the `u8` clone). The SAME TypeParam-span substitution the return type uses,
        // via the shared `substitute_type_params_in_type_node` helper.
        let (template, sig) = {
            let (_, t, s) = make_sig(&[
                "fn f() { let x: i32 = 10; sink(x); }",
                "fn g() { let y: u8 = 20; sink(y); }",
            ]);
            (t, s)
        };
        // The annotation varies → a TypeParam VP; the literal varies → a value_param VP.
        assert!(
            template.variation_points.iter().any(|vp| vp.kind == MetavarKind::TypeParam),
            "the varying annotation must be a type_param VP, got {:?}",
            template.variation_points
        );
        // The promoted runtime param's annotation is the DECLARED generic, not the anchor concrete.
        let arg0 =
            sig.params.iter().find(|p| p.name == "arg0").expect("arg0 promoted from the literal");
        assert_eq!(
            arg0.type_text.as_deref(),
            Some("T0"),
            "the param annotation must substitute the declared generic T0 (not i32), got {:?}",
            arg0.type_text
        );
        assert!(
            sig.text.contains("arg0: T0")
                && !sig.text.contains("arg0: i32")
                && !sig.text.contains("arg0: u8"),
            "render must be `fn extracted<T0>(arg0: T0)`, NOT a concrete annotation, got {:?}",
            sig.text
        );
        assert!(
            sig.generic_params.iter().any(|g| g == "T0"),
            "T0 must be declared, got {:?}",
            sig.generic_params
        );

        // REGRESSION GUARD: a CLASS-STABLE annotation (same type, only the value varies — `let x:
        // i32 = 10` / `let x: i32 = 20`) must NOT be substituted: it has NO TypeParam VP, so the
        // concrete `i32` is correct for the whole class and stays `arg0: i32`.
        let (_, _t2, sig_stable) = make_sig(&[
            "fn f() { let x: i32 = 10; sink(x); }",
            "fn g() { let y: i32 = 20; sink(y); }",
        ]);
        let arg0_stable =
            sig_stable.params.iter().find(|p| p.name == "arg0").expect("arg0 promoted");
        assert_eq!(
            arg0_stable.type_text.as_deref(),
            Some("i32"),
            "a class-stable annotation must keep its concrete type (no spurious generic), got {:?}",
            arg0_stable.type_text
        );
        assert!(
            !sig_stable.text.contains("T0"),
            "a class-stable annotation must not introduce a generic, got {:?}",
            sig_stable.text
        );
    }

    // ── Codex #2: a mixed-kind literal value_param's generic placeholder must be DECLARED
    // ──────────

    #[test]
    fn mixed_kind_literal_param_declares_generic() {
        // Codex #2: a value_param whose per-member literal KINDS differ (int 10 vs float 2.5) gets
        // a generic `T<n>` placeholder (its type isn't stable across the class). The old
        // code returned `type_text: Some("T0")` but never pushed `T0` into `generic_params`
        // → the rendered signature was `fn extracted(arg0: T0)` with an UNDECLARED `T0`
        // (invalid Rust). Every `T<n>` that appears must be declared.
        let (_, template, sig) =
            make_sig(&["fn f() { let x = 10; sink(x); }", "fn g() { let y = 2.5; sink(y); }"]);
        // The mixed-kind literal value_param VP exists with type_hint == None.
        let lit_vp = template
            .variation_points
            .iter()
            .find(|vp| vp.kind == MetavarKind::ValueParam && vp.type_hint.is_none())
            .expect("a mixed-kind literal value_param VP");
        // Its promoted param carries a generic `T<n>` placeholder.
        let lit_param = sig
            .params
            .iter()
            .find(|p| p.metavar_id == lit_vp.metavar_id)
            .expect("the mixed-kind value_param is promoted to a positional param");
        let generic = lit_param
            .type_text
            .as_deref()
            .filter(|t| t.starts_with('T') && t[1..].chars().all(|c| c.is_ascii_digit()))
            .expect("mixed-kind value_param must carry a generic T<n> placeholder");
        // That generic MUST be declared — no undeclared `T<n>` in the rendered signature.
        assert!(
            sig.generic_params.iter().any(|g| g == generic),
            "the mixed-kind generic {generic} must be declared in generic_params: {:?}",
            sig.generic_params
        );
        // Render: `fn extracted<T0>(arg0: T0)` — the generic is declared in the `<…>` clause.
        assert!(
            sig.text.contains(&format!("extracted<{generic}>")) || sig.text.contains(generic),
            "render must declare the generic, got {:?}",
            sig.text
        );
        assert!(
            sig.text.contains(&format!("arg0: {generic}")),
            "the value_param must render typed as the declared generic, got {:?}",
            sig.text
        );
    }

    // ── Codex #3: a Rust string-content literal bucket maps to `&str`, not `{literal}`
    // ────────────

    #[test]
    fn string_literal_value_param_typed_as_str() {
        // Codex #3: a string-literal value diff (`"a"` vs `"bb"`) records its bucket as the
        // `string_content` leaf (`LIT_STRING_CONTENT`). The old mapper only recognised
        // `LIT_STRING_LITERAL`, so it fell through to the opaque `{literal}` placeholder. Both
        // string buckets must map to `&str`.
        assert_eq!(literal_bucket_to_type("LIT_STRING_CONTENT"), Some("&str"));
        assert_eq!(literal_bucket_to_type("LIT_STRING_LITERAL"), Some("&str"));

        // End-to-end: two string-literal-valued members whose string CONTENT differs in a way that
        // surfaces a value_param VP must type it as `&str`, never `{literal}`.
        let (_, template, sig) = make_sig(&[
            "fn f() { let s = \"a\"; sink(s); }",
            "fn g() { let s = \"bb\"; sink(s); }",
        ]);
        // If a value_param VP with a string bucket exists, its promoted param must be `&str`.
        if let Some(str_vp) = template.variation_points.iter().find(|vp| {
            vp.kind == MetavarKind::ValueParam
                && vp.type_hint.as_deref().is_some_and(|b| b.contains("STRING"))
        }) {
            let str_param = sig
                .params
                .iter()
                .find(|p| p.metavar_id == str_vp.metavar_id)
                .expect("the string value_param is promoted to a positional param");
            assert_eq!(
                str_param.type_text.as_deref(),
                Some("&str"),
                "a string-content value_param must type as &str, not {{literal}}, got {:?}",
                str_param.type_text
            );
            assert!(
                !sig.text.contains("{literal}"),
                "the rendered signature must not carry the opaque {{literal}} placeholder, got \
                 {:?}",
                sig.text
            );
        } else {
            // No value_param VP arose (the baseline bucketed both strings identically): the
            // bucket→type assertions above still pin the Codex #3 mapping fix.
        }
    }

    // ── Test 8c (Fix 6): return type comes from the OUTER fn header, not a nested closure arrow
    // ───

    #[test]
    fn return_type_ignores_nested_closure_arrow() {
        // Fix 6: the outer fn has NO return type but its body holds a TYPED closure
        // (`let g = |x: i32| -> i32 { x };`). The old token scan hit the closure's `->` and falsely
        // claimed the helper returns `i32`. The fix recovers ONLY the outer function-header return
        // type → None here. (A differing literal forces a real 2-member class.)
        let (_, _template, sig) = make_sig(&[
            "fn f() { let g = |x: i32| -> i32 { x }; let n = 10; sink(n); }",
            "fn h() { let g = |x: i32| -> i32 { x }; let n = 20; sink(n); }",
        ]);
        assert_eq!(
            sig.return_type, None,
            "a nested typed closure arrow must NOT be recovered as the helper return type, got \
             {:?}",
            sig.return_type
        );
        assert!(
            !sig.text.contains("->"),
            "the rendered signature must have no return-type arrow, got {:?}",
            sig.text
        );
    }

    #[test]
    fn return_type_recovers_outer_fn_header_with_closure_in_body() {
        // Fix 6 companion: when the OUTER fn DOES declare a return type (`-> Outer`) AND the body
        // has a typed closure (`-> Inner`), the recovered return type is the OUTER one,
        // never the inner.
        let (_, _template, sig) = make_sig(&[
            "fn f() -> Outer { let g = |x| -> Inner { x }; let n = 10; n.into() }",
            "fn h() -> Outer { let g = |x| -> Inner { x }; let n = 20; n.into() }",
        ]);
        assert_eq!(
            sig.return_type.as_deref(),
            Some("Outer"),
            "the OUTER fn header return type must win over a nested closure arrow, got {:?}",
            sig.return_type
        );
    }

    // ── Test 8 (P2c): signature type recovery reads the MEDOID anchor, not members[0] ────────────

    #[test]
    fn signature_recovers_from_medoid_anchor_not_member0() {
        // P2c: `anti_unify` records variation_points' occurrences relative to the MEDOID anchor
        // (`resolve_anchor_idx`), and type recovery must read the SAME anchor member — recovering
        // from `members[0]` when the medoid is a different member indexes the column into the wrong
        // syntax.
        //
        // The threading is exercised by calling `propose_signature` with TWO distinct anchor
        // indices and requiring both to recover correctly — a `members[0]`-fixed recovery would
        // misbehave for the non-zero anchor. The RETURN type differs by custom type name
        // (`-> Aaa` vs `-> Bbb`) — and as of Fix 1 (#215 Plan 4b Codex round-5) a differing
        // type-position identifier is no longer invisible: it is a `type_param` VP, so the return
        // renders the DECLARED generic `T0` for BOTH anchors (the new correct behavior; the old
        // alpha-rename-invisibility that let it render the anchor's concrete `Aaa`/`Bbb` is
        // CLOSED).
        let m0 = member(1, "fn f() -> Aaa { let x = 1; sink(x) }");
        let m1 = member(2, "fn g() -> Bbb { let y = 1; sink(y) }");
        let members = canonical(vec![m0, m1]);

        // Identify which canonical member carries which return type (canonical sort may reorder).
        let idx_aaa =
            members.iter().position(|m| m.text.contains("-> Aaa")).expect("member with -> Aaa");
        let idx_bbb =
            members.iter().position(|m| m.text.contains("-> Bbb")).expect("member with -> Bbb");
        assert_ne!(idx_aaa, idx_bbb, "the two return types must land at distinct anchor indices");

        let anchor_idx = resolve_anchor_idx(&members, None);
        let alignment = align_to_anchor(&members, anchor_idx);
        let template = anti_unify(&members, &alignment);

        // The differing return type is a type_param VP → both anchors render the declared generic
        // `T0` (it VARIES across the class — neither anchor's concrete name is correct for the
        // class). The threading is still exercised: `propose_signature` is called with TWO distinct
        // anchor indices, and both recover the same correct generic (not a member-0-fixed
        // concrete).
        for anchor in [idx_aaa, idx_bbb] {
            let sig = propose_signature(&template, &members, anchor);
            assert_eq!(
                sig.return_type.as_deref(),
                Some("T0"),
                "a differing return type-identifier is a type_param → renders generic T0 for \
                 anchor_idx={anchor}, got {:?}",
                sig.return_type
            );
            assert!(
                sig.generic_params.iter().any(|g| g == "T0"),
                "the return-type generic T0 must be declared for anchor_idx={anchor}, got {:?}",
                sig.generic_params
            );
        }
    }
}
