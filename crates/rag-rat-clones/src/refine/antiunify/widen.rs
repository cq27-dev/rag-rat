use super::super::RefineMember;
use super::classify::is_type_position;
use super::spans::subtree_token_count;

/// The string-BODY leaf kinds whose value the baseline normalizer erases (buckets to
/// `LIT_STRING_CONTENT` / `LIT_STRING_FRAGMENT`): Rust/Python `string_content` and TS/JS
/// `string_fragment` (#232 #2a). Both are the inner text leaf that needs widening to its enclosing
/// quote-bearing node so the hole covers the WHOLE `"hello"`.
///
/// Swift's are `line_str_text` / `multi_line_str_text` (the `"…"` and `"""…"""` bodies) and
/// `raw_str_part` / `raw_str_end_part` (the `#"…"#` body — an UNINTERPOLATED raw string arrives as
/// a single `raw_str_end_part` leaf carrying its own delimiters). They must widen for the same
/// reason every other language's do: a hole over the bare text leaf, with the quotes left as fixed
/// template text, does not describe the `&str` value that actually varies.
fn is_string_body_leaf_kind(kind: &str) -> bool {
    matches!(
        kind,
        "string_content"
            | "string_fragment"
            | "line_str_text"
            | "multi_line_str_text"
            | "raw_str_part"
            | "raw_str_end_part"
    )
}

/// The quote-bearing string-NODE kinds that wrap a string-body leaf: Rust/Python `string_literal`,
/// TS/JS `string` (#232 #2a), and TS/JS `template_string` — the `` `…` `` template literal (#254).
///
/// A `template_string` is admitted here so a differing `string_fragment` inside a backtick literal
/// (`` `hi` `` vs `` `lo` ``) widens to the WHOLE `` `hi` `` (backticks included), exactly like a
/// `"…"`. CAUTION — an INTERPOLATED template (`` `hi${x}lo` ``) contains a `template_substitution`
/// child; widening a fragment run to the whole `template_string` there would SWALLOW the `${x}`
/// interpolation into the hole. The single caller [`widen_string_content_run`] guards against that
/// by REFUSING to widen to any string node whose subtree carries a `template_substitution` — the
/// bare fragment run is left as-is (an honest under-report, never an over-claim). So this predicate
/// stays a simple kind test; the interpolation safety lives in the widen gate.
///
/// Swift's quote-bearing nodes are `line_string_literal` (`"…"`), `multi_line_string_literal`
/// (`"""…"""`), and `raw_string_literal` (`#"…"#`). They carry the SAME interpolation hazard as a
/// template string — Swift's `\(x)` — so the widen gate rejects them on the same grounds; see
/// [`string_node_has_interpolation`].
pub(super) fn is_string_node_kind(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "string"
            | "template_string"
            | "line_string_literal"
            | "multi_line_string_literal"
            | "raw_string_literal"
    )
}

/// `true` for a leaf that can legitimately sit INSIDE a quote-bearing string node — either the
/// value-erased body leaf (`string_content` / `string_fragment`) or one of the DELIMITER leaves the
/// grammar emits around it. The delimiter set covers the empty-literal case (#274 item 16): an
/// empty `""` / `` `` `` has NO body leaf, only the two quote/backtick leaves, so a `""`-vs-`"x"`
/// diff surfaces the variation run on a QUOTE leaf rather than a `string_content`. Admitting quote
/// leaves lets that run still widen to the whole literal (the quotes are part of the `&str` value),
/// instead of leaving a stray `"`/`` ` `` as fixed template text. Escape-sequence leaves (`\n`)
/// inside a non-empty literal are also string-internal and widen the same way.
///
/// NOT a `template_substitution` delimiter (`${` / `}`): those bound an interpolation, which the
/// widen gate refuses to cross — see [`is_string_node_kind`]. Same for Swift's `\(` interpolation
/// start; its `"""` multi-line delimiter and `str_escaped_char` escape leaf ARE string-internal and
/// widen like every other grammar's.
fn is_string_delimiter_or_body_leaf_kind(kind: &str) -> bool {
    is_string_body_leaf_kind(kind)
        || matches!(kind, "\"" | "`" | "'" | "\"\"\"" | "escape_sequence" | "str_escaped_char")
}

/// `true` when the anchor subtree rooted at `str_col` contains an interpolation — a TS/JS
/// `template_substitution` (`` `hi${x}lo` ``), or Swift's `interpolated_expression` /
/// `raw_str_interpolation` (`"hi\(x)lo"`, `#"r\#(y)s"#`). Widening a fragment/quote run to such a
/// node would swallow the interpolated EXPRESSION — live code, not a value — into the hole, so
/// [`widen_string_content_run`] does not widen across it (#254 caution). Walks the contiguous
/// pre-order subtree by byte containment, mirroring [`subtree_token_count`].
fn string_node_has_interpolation(anchor: &RefineMember, str_col: usize) -> bool {
    let root_end = anchor.node_spans[str_col].end_byte;
    let mut k = str_col + 1;
    while k < anchor.node_spans.len() && anchor.node_spans[k].start_byte < root_end {
        if matches!(
            anchor.node_spans[k].kind,
            "template_substitution" | "interpolated_expression" | "raw_str_interpolation"
        ) {
            return true;
        }
        k += 1;
    }
    false
}

/// Widen a Raw run that is a single string-internal LEAF — the value-erased body
/// (`string_content` / `string_fragment`) OR a delimiter/quote leaf of an empty literal — to the
/// enclosing quote-bearing string NODE's whole span, so the hole covers the WHOLE `"hello"` /
/// `` `hi` `` (quotes included) — not just the inner text (Fix 3, #215 Plan 4b Codex round-5;
/// extended to TS/JS `string` in #232 #2a; to TS/JS `template_string` and to the empty-literal
/// quote run in #254 / #274 items 16). A non-string run is returned unchanged.
///
/// Reconstructs the enclosing string node from the pre-order `node_spans` (no parent pointers): the
/// tightest `string_literal` / `string` / `template_string` node whose byte span CONTAINS the leaf,
/// then the contiguous pre-order token range of that node (its root column through the last column
/// inside its byte span). Falls back to the original run if no enclosing string node is found
/// (defensive — a bare body leaf with no wrapper, which the grammars never emit).
///
/// INTERPOLATION SAFETY (#254): an interpolated `` `hi${x}lo` `` is NEVER widened — the chosen
/// string node would carry a `template_substitution`, so widening would swallow the `${x}` into the
/// hole. Such a node is rejected (`string_node_has_interpolation`) and the bare fragment run is
/// left as-is: an honest under-report (a stray backtick at worst), never an over-claim across the
/// interpolation boundary.
pub(super) fn widen_string_content_run(
    anchor: &RefineMember,
    lo: usize,
    hi: usize,
) -> (usize, usize) {
    // Only a single string-internal leaf needs widening — a wider run already covers its quotes.
    if lo != hi {
        return (lo, hi);
    }
    let leaf = &anchor.node_spans[lo];
    if !(leaf.is_leaf && is_string_delimiter_or_body_leaf_kind(leaf.kind)) {
        return (lo, hi);
    }
    // Tightest enclosing string node (smallest byte span containing the leaf) that is NOT an
    // interpolated template (widening across a `${…}` would swallow the interpolation — #254).
    let mut best: Option<usize> = None;
    let mut best_width = usize::MAX;
    for (c, sp) in anchor.node_spans.iter().enumerate() {
        if !is_string_node_kind(sp.kind) {
            continue;
        }
        if sp.start_byte <= leaf.start_byte && leaf.end_byte <= sp.end_byte {
            let width = sp.end_byte.saturating_sub(sp.start_byte);
            if width < best_width && !string_node_has_interpolation(anchor, c) {
                best_width = width;
                best = Some(c);
            }
        }
    }
    let Some(str_col) = best else { return (lo, hi) };
    // The whole pre-order token range of the string node subtree.
    let new_hi = str_col + subtree_token_count(anchor, str_col) - 1;
    (str_col, new_hi)
}

/// Widen a Raw run that is the HEAD type-name leaf of an enclosing `generic_type` to the WHOLE
/// `generic_type` node (Fix 4, #215 Plan 4b Codex round-7). A non-head / non-`generic_type` run is
/// returned unchanged.
///
/// `Vec<i32>` vs `Option<i32>` reopens (via [`matched_column_reopen`] `TypeParam`) only the head
/// `type_identifier` leaf `Vec`/`Option` — its byte span is JUST the name, so the type ARGS
/// (`<i32>`) render as FIXED text around the hole → `⟨m0⟩<i32>` and the signature `-> T0<i32>`
/// (INVALID Rust, and it hard-codes the anchor's type args). Widening to the enclosing
/// `generic_type` makes the WHOLE `Vec<i32>` one `type_param` hole → `⟨m0⟩` / generic `T0` with
/// `per_member_values = [Vec<i32>, Option<i32>]`, the only valid generalization when the type HEAD
/// differs.
///
/// The HEAD test (`generic_type.start_byte == leaf.start_byte`) is what distinguishes this from the
/// existing INNER-arg case: `Vec<i32>` vs `Vec<u8>` reopens the inner `i32`/`u8` leaf, whose
/// `start_byte` is INSIDE the `generic_type` (after `Vec<`), so it is NOT a head and is left as the
/// inner-leaf hole (`Vec<⟨m0⟩>` — the round-3 wrapper-preserving case). Mirrors
/// [`widen_string_content_run`]: same single-leaf gate + tightest-enclosing-subtree + whole pre-order
/// token range. Recovered `per_member_values` come from the widened span's real byte offsets, so
/// the member that is `Option<i32>` recovers `Option<i32>`, not just `Option`.
pub(super) fn widen_generic_type_head_run(
    anchor: &RefineMember,
    lo: usize,
    hi: usize,
) -> (usize, usize) {
    // Only a single bare type-name leaf needs widening — a wider run already covers its args.
    if lo != hi {
        return (lo, hi);
    }
    let leaf = &anchor.node_spans[lo];
    if !(leaf.is_leaf && matches!(leaf.kind, "type_identifier" | "scoped_type_identifier")) {
        return (lo, hi);
    }
    // Tightest enclosing `generic_type` whose HEAD is this leaf (same start_byte = the type name
    // at the front of `Name<…>`, NOT an inner type argument).
    let mut best: Option<usize> = None;
    let mut best_width = usize::MAX;
    for (c, sp) in anchor.node_spans.iter().enumerate() {
        if sp.kind != "generic_type" {
            continue;
        }
        if sp.start_byte == leaf.start_byte && leaf.end_byte <= sp.end_byte {
            let width = sp.end_byte.saturating_sub(sp.start_byte);
            if width < best_width {
                best_width = width;
                best = Some(c);
            }
        }
    }
    let Some(gen_col) = best else { return (lo, hi) };
    // The whole pre-order token range of the generic_type subtree.
    let new_hi = gen_col + subtree_token_count(anchor, gen_col) - 1;
    (gen_col, new_hi)
}

/// The syntactic type context at anchor column `lo` — the recovered `: T` annotation text in the
/// anchor source, or `None` when there is no nearby annotation (Fix 3, Codex round-7). Used ONLY as
/// part of the [`collapse_recurring`] key, so two occurrences with the same value tuple + role but
/// DIFFERENT annotations (`let a: i32 = 1` / `let b: u8 = 1`) stay separate metavars.
///
/// Scans a small window before `lo` for a `:` leaf, then forward for a type-position node, and
/// returns that node's real source text — the same shape as `signature::try_annotation_type_span`
/// (the recovery side), kept here as a tiny collapse-key probe rather than a cross-module call. The
/// goal is DISTINCTNESS, not perfect recovery: if the scan misses, both occurrences get `None` and
/// fall back to the prior (value+role) collapse, never a worse outcome than before the fix.
pub(super) fn annotation_type_context(anchor: &RefineMember, lo: usize) -> Option<String> {
    let spans = &anchor.node_spans;
    if spans.is_empty() {
        return None;
    }
    let lo = lo.min(spans.len() - 1);
    let window_start = lo.saturating_sub(6);
    for colon_idx in (window_start..lo).rev() {
        let span = &spans[colon_idx];
        if !(span.is_leaf && span.kind == ":") {
            continue;
        }
        let search_end = (colon_idx + 8).min(spans.len());
        for tspan in spans.iter().take(search_end).skip(colon_idx + 1) {
            if is_type_position(tspan.kind) {
                return anchor.text.get(tspan.start_byte..tspan.end_byte).map(str::to_string);
            }
        }
        break;
    }
    None
}
