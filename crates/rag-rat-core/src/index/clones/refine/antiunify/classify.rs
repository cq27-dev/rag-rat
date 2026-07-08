use super::super::RefineMember;
use super::super::score::Confidence;
use super::types::{ClassAlignment, MetavarKind};
use super::values::aligned_values;
use super::widen::is_string_node_kind;
use crate::index::clones::normalize::NodeSpan;

/// The role a reopened matched column takes — the output of [`matched_column_reopen`]. Each variant
/// names a position the baseline normalizer ERASES the source value of, so a genuine difference
/// hides behind an LCS-matched (and therefore "fixed") column. `classify_run` independently derives
/// the same role from the anchor node kind / callee position, so this is the reopen-decision side
/// of one source of truth, not a second classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReopenRole {
    /// A value-erased literal leaf (every literal buckets to its `LIT_<KIND>` token). Classifies as
    /// `value_param` (Fix 2).
    ValueLiteral,
    /// A type-position identifier leaf (a custom type NAME alpha-renames to `ID<n>` like a local).
    /// Classifies as `type_param`, rendered as a generic (Fix 1, Codex round-5).
    TypeParam,
    /// A callee/method-name identifier leaf in call-head position (a single-use free callee or a
    /// method-name head alpha-renames to the same `ID<n>` across members). Classifies as
    /// `closure_param` with `differing_callee=true` (Codex round-6).
    Callee,
}

/// Decide whether the matched (LCS-"fixed") anchor column `col` must REOPEN as a variation — and in
/// what role. This is the SINGLE place that owns "what positions reopen, and why"; both the literal
/// reopen (Fix 2), the type-identifier reopen (Fix 1, Codex round-5), and the callee-identifier
/// reopen (Codex round-6) route through here, so the leak each one patched can't recur unnoticed at
/// a new position. Returns `None` when the column should stay fixed.
///
/// A column reopens ONLY when BOTH hold:
/// 1. its anchor leaf sits in a value-erased position the normalizer collapsed (see the audit
///    below), AND
/// 2. the ALIGNED members' RECOVERED source values at `col` are NOT all byte-equal
///    ([`matched_source_values_differ`]) — i.e. the erasure actually hides a difference. (When the
///    values are equal there is genuinely nothing to reopen; this is the C1 invariant up front.)
///
/// # Position audit — what reopens, what stays fixed, and WHY
///
/// REOPEN (a normalization-erased source value hides a real clone difference here):
/// - **Literal leaf** → [`ReopenRole::ValueLiteral`]. Every literal buckets to its KIND
///   (`LIT_INTEGER_LITERAL`, …); the value is erased, so `let x = 10` vs `let x = 20` match. A
///   clean by-value hole.
/// - **Type-position identifier leaf** → [`ReopenRole::TypeParam`]. A custom type NAME
///   alpha-renames to `ID<n>` exactly like a local, so `let x: Foo` vs `let x: Bar` match. A
///   DIFFERING type IS a real variation (a generic), unlike a consistently-renamed value local.
///   Scoped on the leaf NODE KIND ([`is_type_position`]), never the `ID` token.
/// - **Callee / method-name identifier leaf in call-head position** → [`ReopenRole::Callee`]. A
///   single-use free callee `foo()` vs `bar()` (and a method-name head `x.foo()` vs `x.bar()`) both
///   alpha-rename to the same `ID<n>` / field-name, so they LCS-match and read fixed. A differing
///   callee is a DIFFERENT FUNCTION the baseline cannot prove resolves to the same symbol (the
///   Plan-3 SCIP seam), so it must surface as `closure_param`/`differing_callee` — exactly the
///   differing-callee guard's verdict, which never fired before because no variation run reached
///   it. Detected with the SAME [`run_in_callee_position`] the guard uses (the free-fn callee head
///   and the `field_identifier` method-name head); the method RECEIVER and call ARGUMENTS are
///   excluded there, so they are NOT reopened as callees.
///
/// MACRO-INVOCATION NAME (`foo!()` vs `bar!()`): COVERED by the callee branch. The differing-callee
/// guard treats `macro_invocation` as a call head ([`opens_call_head`]), and a macro callee leaf in
/// `macro_invocation` head position satisfies [`run_in_callee_position`] — so a differing macro
/// name reopens as [`ReopenRole::Callee`] like a free-fn callee. (In practice the macro `!` bang
/// makes the path-name leaf differ structurally more often than a bare fn, but the head-position
/// reopen covers the alpha-renamed-collision case symmetrically.)
///
/// DELIBERATELY NOT REOPENED (a matched column here is genuinely fixed — reopening would
/// manufacture false variation):
/// - **Value-position local identifier** (`x` consistently renamed to `y`, same value). A
///   consistent alpha-rename is the intended Type-2 equivalence — the load-bearing distinction this
///   fix must preserve. `matched_column_reopen` returns `None` (it is neither a literal, nor a type
///   position, nor a callee position). The differing-callee-via-ID-collapse case is the ONE
///   value-position identifier exception, handled above precisely because it is a callee, not a
///   plain value.
/// - **Plain field access** `x.foo` vs `x.bar` (a `field_expression` field that is NOT a call). The
///   field name is a member access, not a callee; the baseline cannot distinguish it from a renamed
///   value field, so treating it as variation would over-fire on alpha-rename. Left fixed.
///   ([`run_in_callee_position`] only matches a `field_identifier` that is a call's method-name
///   head, not a bare field access.)
/// - **Struct field names, labels (`'a:`), lifetimes (`'a`)**. These alpha-rename consistently like
///   value locals (a renamed lifetime/label/field is a Type-2 equivalent, not a clone difference),
///   and none is a literal / type-name / callee position, so the reopen never fires on them. Left
///   fixed deliberately — same alpha-rename-equivalence rationale as value locals.
pub(super) fn matched_column_reopen(
    anchor: &RefineMember,
    col: usize,
    members: &[RefineMember],
    alignment: &ClassAlignment,
) -> Option<ReopenRole> {
    let role = if anchor_leaf_is_literal(anchor, col) {
        ReopenRole::ValueLiteral
    } else if anchor_leaf_is_type_identifier(anchor, col) {
        ReopenRole::TypeParam
    } else if anchor_leaf_is_callee_identifier(anchor, col) {
        ReopenRole::Callee
    } else {
        return None;
    };
    // The erasure only matters when the recovered source values actually differ (C1 up front): a
    // same-callee / same-type / same-literal column stays fixed.
    if !matched_source_values_differ(members, alignment, col) {
        return None;
    }
    // SCIP moniker collapse (#275, Plan 3): a callee column whose members SPELL the callee
    // differently but all RESOLVE to one oracle moniker is the same function (moved / aliased /
    // re-exported) — the Type-2 equivalence the baseline can't prove. The column stays FIXED
    // (anchor's spelling renders), exactly like a consistently-renamed value local; no variation
    // point, no `differing_callee`. Only fires when EVERY contributing member carries a moniker
    // for its span (attached only in scip refine mode) — one missing resolution vetoes the
    // collapse, so oracle staleness degrades to today's conservative reopen, never a wrong fix.
    if role == ReopenRole::Callee && matched_callee_monikers_agree(members, alignment, col) {
        return None;
    }
    Some(role)
}

/// `true` when EVERY aligned member contributing a token at matched column `col` carries a SCIP
/// callee moniker for that token's span, and all those monikers name ONE symbol (#275, Plan 3).
/// A contributing member WITHOUT a moniker returns `false` (no oracle evidence → no collapse);
/// members that gapped the column contribute no opinion, mirroring
/// [`matched_source_values_differ`]. `local N` monikers never reach here
/// (`current_callee_monikers` drops them — document-scoped identity must not equate across
/// files).
fn matched_callee_monikers_agree(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    col: usize,
) -> bool {
    let mut seen: Option<&str> = None;
    for (m_idx, member) in members.iter().enumerate() {
        if !alignment.aligned[m_idx] {
            continue;
        }
        let Some(j) = alignment.col_map[m_idx][col] else { continue };
        let span = &member.node_spans[j];
        let Some(moniker) = member.callee_monikers.get(&(span.start_byte, span.end_byte)) else {
            return false;
        };
        match seen {
            None => seen = Some(moniker),
            Some(prev) if prev == moniker => {},
            Some(_) => return false,
        }
    }
    seen.is_some()
}

/// `true` when the anchor's spine leaf at column `col` is a callee / method-name identifier in
/// call-head position — the THIRD value-erased matched-column position ([`ReopenRole::Callee`]).
/// A single-use free callee, a method-name head, or a macro name alpha-renames to the same `ID<n>`
/// like a value local, so a matched column here can hide a differing callee (a different function
/// the baseline can't prove resolves to the same symbol — the Plan-3 SCIP seam). Gated on the leaf
/// being a callee-leaf kind AND in callee position via the SAME [`run_in_callee_position`] the
/// differing-callee guard uses (free-fn head, method-name `field_identifier` head; receiver and
/// arguments excluded) — so a value-position local, a plain field access, or a call argument is
/// NEVER treated as a callee here (Codex round-6).
fn anchor_leaf_is_callee_identifier(anchor: &RefineMember, col: usize) -> bool {
    anchor.node_spans.get(col).is_some_and(|sp| sp.is_leaf && is_callee_leaf_kind(sp.kind))
        && run_in_callee_position(anchor, col, col)
}

/// `true` when the anchor's spine leaf at column `col` is a value-erased literal bucket (`LIT_*`).
/// The normalizer buckets every literal to its KIND, dropping the value — so a `LIT_*` token at a
/// matched column may hide a real source-value difference across members (Fix 2).
fn anchor_leaf_is_literal(anchor: &RefineMember, col: usize) -> bool {
    anchor.node_spans.get(col).is_some_and(|sp| sp.is_leaf)
        && anchor.seq.get(col).is_some_and(|tok| tok.starts_with("LIT_"))
}

/// `true` when the anchor's spine leaf at column `col` is a TYPE-POSITION identifier — a leaf whose
/// NODE KIND is a type position (`type_identifier` / `scoped_type_identifier`). A custom type name
/// alpha-renames to `ID<n>` exactly like a value local, so a matched column here can hide a real
/// type-NAME difference across members (`let x: Foo` vs `let x: Bar`). Gating on the node KIND (not
/// the `ID` token) is what keeps Fix 1 SCOPED STRICTLY to type positions — a value-position `ID`
/// leaf (a consistently alpha-renamed local) is NEVER flipped (#215 Plan 4b Codex round-5).
fn anchor_leaf_is_type_identifier(anchor: &RefineMember, col: usize) -> bool {
    anchor.node_spans.get(col).is_some_and(|sp| sp.is_leaf && is_type_position(sp.kind))
}

/// `true` when the aligned members' RECOVERED SOURCE VALUES at matched column `col` are NOT all
/// byte-equal — i.e. a value-erased leaf (a literal bucket, a type-position identifier, OR a
/// callee/method-name identifier) hides a genuine source difference. The "is this an erased
/// position?" decision lives in [`matched_column_reopen`]; this only answers "do the values
/// differ?".
///
/// Only consults ALIGNED members that matched a token at `col` (an un-aligned / cost-skipped member
/// contributes no value; a member that gapped the column can't be compared and is ignored — the
/// caller only flips columns the base fixedness already deemed matched-by-all-aligned). Recovers
/// each member's source via the same `text.get(span..)` UTF-8-guarded path as `recover_values`; a
/// slice miss is treated as "no opinion" (skipped) so a UTF-8 edge can't manufacture a variation.
fn matched_source_values_differ(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    col: usize,
) -> bool {
    let mut seen: Option<&str> = None;
    for (m_idx, member) in members.iter().enumerate() {
        if !alignment.aligned[m_idx] {
            continue;
        }
        let Some(j) = alignment.col_map[m_idx][col] else { continue };
        let span = &member.node_spans[j];
        let Some(value) = member.text.get(span.start_byte..span.end_byte) else { continue };
        match seen {
            None => seen = Some(value),
            Some(prev) if prev == value => {},
            Some(_) => return true,
        }
    }
    false
}

/// Classification outcome for one run: its extraction role, an optional type hint, the confidence
/// band, and whether the differing-callee guard fired (the Plan-3 SCIP seam).
pub(super) struct RunClass {
    pub(super) kind: MetavarKind,
    pub(super) type_hint: Option<String>,
    pub(super) confidence: Confidence,
    /// `true` ONLY when the differing-callee guard fired (a genuine differing callee/method-name).
    /// Carried explicitly so downstream scoring never re-infers it from `(kind, confidence)` — Fix
    /// 5 (a `ClosureParam` at `Medium` may instead be a generic closure-ish subtree).
    pub(super) differing_callee: bool,
}

/// Classify a run's extraction role (§1.8), precedence `gapped > closure_param > type_param >
/// value_param`.
pub(super) fn classify_run(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    anchor: &RefineMember,
    lo: usize,
    hi: usize,
    per_member_values: &[String],
) -> RunClass {
    // (1) gapped — any ALIGNED member gaps the run (empty value). Low.
    // Skipped (cost-capped) members are excluded via `aligned_values`: their `""` is "value unknown
    // (too long to align)", NOT a genuine indel gap — a skipped member must not demote an
    // otherwise-clean ValueParam/TypeParam/ClosureParam run to Gapped. Same aligned-only filter as
    // the C1 guard, `is_fixed`, `subtree_is_indel`, and `matched_literal_values_differ`.
    if aligned_values(per_member_values, alignment).any(|v| v.is_empty()) {
        return RunClass {
            kind: MetavarKind::Gapped,
            type_hint: None,
            confidence: Confidence::Low,
            differing_callee: false,
        };
    }

    let run_len = hi - lo + 1;
    let anchor_kind = anchor.node_spans[lo].kind;

    // Differing-callee guard (the Plan-3 SCIP seam): a run whose anchor node is a call/method/macro
    // head — OR sits in the callee/function-head POSITION of an enclosing call node (a bare
    // `identifier`/`field_identifier`/`scoped_identifier` leaf that is the callee child, the
    // Plan-3 seam surfacing as a LEAF rather than the `call_expression` node) — and whose callee
    // DIFFERS across members is closure_param Medium/Low, NEVER a high-confidence value_param.
    // Baseline cannot prove two differing callees resolve to the same symbol. `differing_callee` is
    // set true ONLY here, so downstream scoring keys off the real seam, not the band (Fix 5).
    //
    // SCIP moniker collapse (#275, Plan 3): when the oracle CAN prove it — every member realises
    // the run as one callee leaf whose attached moniker is the SAME symbol — the guard is skipped
    // and the run classifies through the normal ladder (a single `ID` leaf lands at rule (2)
    // value_param, like any consistently-equivalent rename), never `differing_callee`. Scope is
    // deliberately same-call-syntax only (finding 2): a multi-token or cross-syntax run
    // (`a::b::foo()` vs `foo()`) fails the single-leaf gate and keeps today's verdict.
    if run_callees_differ(per_member_values)
        && !run_callee_monikers_agree(members, alignment, lo, hi)
    {
        if opens_call_head(anchor_kind) {
            // The run snapped to the call node itself — generic differing call subtree, Low.
            return RunClass {
                kind: MetavarKind::ClosureParam,
                type_hint: None,
                confidence: Confidence::Low,
                differing_callee: true,
            };
        }
        if run_in_callee_position(anchor, lo, hi) {
            // The differing callee surfaced as the bare callee leaf in call-head position. We can
            // pin it to a named callee → Medium (tighter than a generic differing subtree).
            return RunClass {
                kind: MetavarKind::ClosureParam,
                type_hint: None,
                confidence: Confidence::Medium,
                differing_callee: true,
            };
        }
    }

    // (2a) value_param — a string-node run widened from a differing string-body leaf (Fix 3;
    //      extended to TS/JS `string` in #232 #2a). The run is the WHOLE `"hello"` node (quotes
    //      included), so the anchor at `lo` is the internal `string_literal` (Rust/Python) or
    //      `string` (TS/JS) node, not a leaf — it cannot reach the single-leaf value_param rule (2)
    //      below. A string literal is a clean by-value `&str` parameter: emit value_param High with
    //      the `LIT_STRING_CONTENT` bucket so the signature recovers `&str` (both string-body
    // buckets      map to `&str` in `literal_bucket_to_type`).
    if is_string_node_kind(anchor_kind) {
        return RunClass {
            kind: MetavarKind::ValueParam,
            type_hint: Some("LIT_STRING_CONTENT".to_string()),
            confidence: Confidence::High,
            differing_callee: false,
        };
    }

    // (2) value_param — every member's run is a single compatible leaf (local ID<n> / LIT_<KIND>).
    //     The anchor's run must be exactly one leaf, and every member must contribute exactly one
    //     leaf token to the run.
    //
    //     A custom TYPE name (`type_identifier` / `scoped_type_identifier`, and `generic_type`
    // inner     names) normalizes to `ID<n>` — `is_identifier_kind` matches `*identifier` — so
    // a     `let x: Foo` vs `let x: Bar` run is an `ID`-leaf that would otherwise be promoted
    // to a     value_param HERE, before the type_param check (3) ever runs. Guard on the
    // anchor's NODE     KIND (`is_type_position`), not the leaf token: a type-position leaf
    // falls through to (3)     and is classified type_param, not value_param.
    if run_len == 1 && anchor.node_spans[lo].is_leaf && !is_type_position(anchor_kind) {
        let anchor_tok = &anchor.seq[lo];
        if is_value_leaf_token(anchor_tok) && every_member_single_leaf(members, alignment, lo, hi) {
            let type_hint = uniform_literal_bucket(members, alignment, lo);
            return RunClass {
                kind: MetavarKind::ValueParam,
                type_hint,
                confidence: Confidence::High,
                differing_callee: false,
            };
        }
    }

    // (3) type_param — anchor node kind is a type position.
    if is_type_position(anchor_kind) {
        // High when single-leaf type identifier; Medium for a multi-token generic subtree.
        let confidence = if run_len == 1 && anchor.node_spans[lo].is_leaf {
            Confidence::High
        } else {
            Confidence::Medium
        };
        return RunClass {
            kind: MetavarKind::TypeParam,
            type_hint: None,
            confidence,
            differing_callee: false,
        };
    }

    // (4) closure_param — a multi-token differing subtree (call/field/method/macro/binary/…) or a
    //     differing path-head. Medium when the anchor subtree is a recognised call-ish kind, else
    //     Low (a generic differing run we can't characterise tightly). NOT a differing callee — the
    //     guard above didn't fire, so `differing_callee` stays false even at Medium.
    let confidence = if opens_call_head(anchor_kind) || is_closureish_kind(anchor_kind) {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    RunClass {
        kind: MetavarKind::ClosureParam,
        type_hint: None,
        confidence,
        differing_callee: false,
    }
}

/// `true` for an anchor node kind that opens a call or method head (the callee/path-head lives
/// inside). Used by the differing-callee guard.
fn opens_call_head(kind: &str) -> bool {
    matches!(kind, "call_expression" | "method_call_expression" | "macro_invocation")
}

/// `true` for a callee-leaf node kind: the kind a function/method/macro callee head surfaces as
/// when the differing material snaps to the bare callee LEAF (not the enclosing `call_expression`).
fn is_callee_leaf_kind(kind: &str) -> bool {
    matches!(kind, "identifier" | "field_identifier" | "scoped_identifier")
}

/// `true` when the anchor run `[lo..=hi]` sits in the CALLEE / function-head position of an
/// enclosing call/method/macro node — the Plan-3 SCIP seam surfacing as a bare callee LEAF rather
/// than the `call_expression` node. Reconstructs callee-position from the pre-order `node_spans`
/// (no parent pointers): the run is a callee-leaf kind, and the tightest STRICTLY-enclosing node
/// is a call-ish head where the run is the ACTUAL callee/method-name child. For `call_expression` /
/// `macro_invocation` a free-fn callee is the first child (`start_byte` equal to the call's
/// `start_byte`); a method-name head is the `field_identifier` (`.map` of `x.map(a)`). The method
/// RECEIVER (`x`) also shares the call's `start_byte` but is NOT the callee — it is excluded (Fix
/// 3: a differing receiver with an unchanged method is a `value_param`, not a `closure_param`
/// callee). A plain operand of a `binary_expression` etc. is NOT a callee-head and is left to fall
/// through to `value_param`.
pub(super) fn run_in_callee_position(anchor: &RefineMember, lo: usize, hi: usize) -> bool {
    let run = &anchor.node_spans[lo];
    if !is_callee_leaf_kind(run.kind) {
        return false;
    }
    let run_start = run.start_byte;
    let run_end = anchor.node_spans[hi].end_byte;

    // Tightest call-ish node STRICTLY enclosing the run (smallest byte span that properly contains
    // `[run_start..run_end]`). Pre-order: scan all columns, keep the narrowest qualifying span.
    let mut best: Option<&NodeSpan> = None;
    for sp in &anchor.node_spans {
        if !opens_call_head(sp.kind) {
            continue;
        }
        let encloses = sp.start_byte <= run_start
            && run_end <= sp.end_byte
            && (sp.start_byte < run_start || run_end < sp.end_byte);
        if !encloses {
            continue;
        }
        let narrower = best
            .map(|b| (sp.end_byte - sp.start_byte) < (b.end_byte - b.start_byte))
            .unwrap_or(true);
        if narrower {
            best = Some(sp);
        }
    }
    // The run is the callee/function head iff it is the ACTUAL callee/method-name child of the
    // enclosing call — NOT the receiver and NOT an argument:
    //
    // - A method-name head is a `field_identifier` in the call's function position (the `.map` of
    //   `x.map(a)`): Rust models it as a `field_expression` field; the synthetic fixture as a bare
    //   `field_identifier` sibling. Either way it is a `field_identifier` NOT inside the call's
    //   `arguments` → method callee.
    // - A free-fn / macro callee shares the call's `start_byte` (`foo(...)` → `foo`) AND is NOT a
    //   method-call RECEIVER. The receiver (`x` of `x.map(a)`) ALSO shares the call's `start_byte`
    //   (the `field_expression`/method call begins at the receiver), so the head-position match
    //   alone caught it — the Fix-3 bug, classifying the changed receiver as a closure_param callee
    //   though the method head is unchanged. A run that has a method-name `field_identifier` head
    //   AFTER it in the call's function position is the receiver, never the callee.
    //
    // A run inside the call's `arguments`/`argument_list` is an argument, never the head (P2b).
    let Some(call) = best else { return false };
    if run_in_arguments_position(anchor, call, run_start, run_end) {
        return false;
    }
    // Method-name head: the run IS a `field_identifier` (the method/field name) under the call.
    if run.kind == "field_identifier" {
        return true;
    }
    // Scoped-path callee head: for `m::foo()` the call's callee child is the `scoped_identifier`
    // `m::foo` (an internal node beginning at the call), so the FINAL segment `foo` (a plain
    // `identifier` ending where that `scoped_identifier` ends) is the actual callee name yet does
    // NOT itself share the call's start byte — only the FIRST segment `m` does. Without this a
    // differing MODULE (`m::foo` vs `n::foo`) reopened the matched column but a differing FUNCTION
    // NAME (`m::foo` vs `m::bar`) did not — the #235 asymmetry. Gated by the SAME no-method-head
    // check as a free callee, so a scoped RECEIVER (`m::obj.foo()`, whose head is the `.foo`
    // `field_identifier`) is excluded — its final segment is the receiver, not the callee.
    if run.kind == "identifier"
        && !call_has_method_name_head(anchor, call, run_end)
        && anchor.node_spans.iter().any(|sp| {
            sp.kind == "scoped_identifier"
                && sp.start_byte == call.start_byte
                && sp.end_byte == run_end
        })
    {
        return true;
    }
    // Otherwise the run is a head candidate only if it begins at the call and the call has NO
    // separate method-name head (a free fn / macro). If the call DOES have a method-name head, a
    // start-byte run is the receiver, not the callee.
    call.start_byte == run_start && !call_has_method_name_head(anchor, call, run_end)
}

/// `true` when the enclosing call `call` has a method-name head (a `field_identifier` in the call's
/// FUNCTION position, i.e. not inside the call's `arguments`) that begins AFTER `run_end` — so a
/// run sharing the call's start byte is the method-call RECEIVER, not the callee (Fix 3). A free
/// function / macro call has no such head, so a start-byte run there is the genuine callee.
fn call_has_method_name_head(anchor: &RefineMember, call: &NodeSpan, run_end: usize) -> bool {
    anchor.node_spans.iter().any(|sp| {
        sp.kind == "field_identifier"
            // inside the call …
            && call.start_byte <= sp.start_byte
            && sp.end_byte <= call.end_byte
            // … in the function-head position (after the receiver the run covers) …
            && sp.start_byte >= run_end
            // … and NOT inside the call's argument list (that would be a field arg, not the head).
            && !run_in_arguments_position(anchor, call, sp.start_byte, sp.end_byte)
    })
}

/// `true` when the run `[run_start..run_end]` is contained in an `arguments`/`argument_list` node
/// nested inside the enclosing call `call` — i.e. the run is an ARGUMENT, not the method-name head.
/// Used to keep the `method_call_expression` callee branch from promoting a differing argument leaf
/// (`obj.map(x)` vs `obj.map(y)`) to a closure_param callee (P2b). Reconstructs the nesting from
/// the pre-order `node_spans` by byte-span containment (no parent pointers): an `arguments`-kind
/// node inside `call` that contains the run.
fn run_in_arguments_position(
    anchor: &RefineMember,
    call: &NodeSpan,
    run_start: usize,
    run_end: usize,
) -> bool {
    anchor.node_spans.iter().any(|sp| {
        matches!(sp.kind, "arguments" | "argument_list")
            // The args node is inside the call …
            && call.start_byte <= sp.start_byte
            && sp.end_byte <= call.end_byte
            // … and the run is inside the args node.
            && sp.start_byte <= run_start
            && run_end <= sp.end_byte
    })
}

/// `true` for a multi-token "closure-ish" differing subtree kind.
fn is_closureish_kind(kind: &str) -> bool {
    matches!(
        kind,
        "call_expression"
            | "method_call_expression"
            | "macro_invocation"
            | "field_expression"
            | "binary_expression"
            | "unary_expression"
            | "index_expression"
            | "scoped_identifier"
            | "generic_function"
    )
}

/// `true` for a tree-sitter type-position node kind. Delegates to the shared
/// [`crate::index::clones::normalize::is_rust_type_kind`] so the anti-unify classifier and the
/// signature recoverer (`signature::is_type_kind`) can never disagree on what counts as a type —
/// notably the OUTER composite type nodes (`&Foo`, `[T; N]`, `(A, B)`, `Box<Foo>`, …) the
/// classifier previously omitted, mis-routing them to `closure_param` (Fix 4, #215 Plan 4b).
pub(super) fn is_type_position(kind: &str) -> bool {
    crate::index::clones::normalize::is_rust_type_kind(kind)
}

/// `true` for a baseline leaf token that names a value-position leaf: a local identifier (`ID<n>`)
/// or a literal bucket (`LIT_<KIND>`).
fn is_value_leaf_token(tok: &str) -> bool {
    tok.starts_with("ID") || tok.starts_with("LIT_")
}

/// Every ALIGNED member must contribute exactly one leaf token to the run (anchor included). Used
/// to gate `value_param`: a run that one member realises as a multi-token subtree is not a clean
/// by-value hole.
///
/// Skipped (cost-capped) members are excluded: their all-`None` col_map would contribute zero
/// tokens here, failing the `idxs.len() == 1` check and demoting every leaf swap to `ClosureParam`
/// — the same class of wrong result the `gapped` check has. Mirror the `alignment.aligned[m]`
/// exclusion used throughout (P1 fix).
fn every_member_single_leaf(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    lo: usize,
    hi: usize,
) -> bool {
    members.iter().enumerate().all(|(m_idx, member)| {
        // Skipped members cannot witness the single-leaf property — ignore them.
        if !alignment.aligned[m_idx] {
            return true;
        }
        let cm = &alignment.col_map[m_idx];
        let inserts = &alignment.member_inserts[m_idx];
        // Gather the member's token indices for the run, same as recover_values.
        let mut idxs: Vec<usize> = Vec::new();
        for &slot in &cm[lo..=hi] {
            if let Some(j) = slot {
                idxs.push(j);
            }
        }
        for (_key, ins) in inserts.range(lo..=hi) {
            idxs.extend(ins.iter().copied());
        }
        idxs.len() == 1 && member.node_spans[idxs[0]].is_leaf
    })
}

/// When every member's single-leaf value is the SAME literal bucket, return it as a `type_hint`
/// (e.g. all `LIT_INTEGER_LITERAL`). Mixed literal kinds, or any identifier, yield `None`.
fn uniform_literal_bucket(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    lo: usize,
) -> Option<String> {
    let mut bucket: Option<&str> = None;
    for (m_idx, member) in members.iter().enumerate() {
        // Skipped (cost-capped) members have an all-gap col_map and no insert keyed at `lo`, so the
        // `found?` below would return `None` and abort the whole bucket inference — even when every
        // ALIGNED member is the same literal kind. Their value is "unknown (too long to align)",
        // not a literal-kind opinion. Exclude them so the stable `i64`/`&str`/… type hint still
        // emits over the aligned subset (mirrors the `alignment.aligned[m]` exclusion in
        // `is_fixed`, `subtree_is_indel`, `classify_run`'s gapped check,
        // `matched_literal_values_differ`, and `every_member_single_leaf`). Fix 1 (#215
        // Plan 4b Codex round-4).
        if !alignment.aligned[m_idx] {
            continue;
        }
        let cm = &alignment.col_map[m_idx];
        let inserts = &alignment.member_inserts[m_idx];
        // The member's single token index for this single-column run.
        let j = if let Some(j) = cm[lo] {
            j
        } else {
            // Anchor column lo is a gap for this member — its substituting leaf is keyed AT column
            // lo.
            let mut found = None;
            for (_k, ins) in inserts.range(lo..=lo) {
                if ins.len() == 1 {
                    found = Some(ins[0]);
                }
            }
            found?
        };
        let tok = member.seq.get(j)?;
        if !tok.starts_with("LIT_") {
            return None;
        }
        match bucket {
            None => bucket = Some(tok),
            Some(b) if b == tok => {},
            Some(_) => return None,
        }
    }
    bucket.map(|b| b.to_string())
}

/// `true` when EVERY aligned member realises the run `[lo..=hi]` as exactly ONE leaf token whose
/// span carries a SCIP callee moniker, and all those monikers name ONE symbol (#275, Plan 3) —
/// the oracle-proven "same function, different spelling" case the differing-callee guard exists
/// to be conservative about. Any member realising the run as a multi-token subtree, a non-leaf,
/// or a leaf WITHOUT a moniker returns `false` (finding 2's same-call-syntax scope + the
/// no-evidence veto); a member that gapped the run contributes no opinion, mirroring
/// [`run_callees_differ`]'s empty-value skip. Token indices are gathered the same way as
/// [`every_member_single_leaf`] (col_map slots + inserts keyed in the run).
fn run_callee_monikers_agree(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    lo: usize,
    hi: usize,
) -> bool {
    let mut seen: Option<&str> = None;
    for (m_idx, member) in members.iter().enumerate() {
        if !alignment.aligned[m_idx] {
            continue;
        }
        let cm = &alignment.col_map[m_idx];
        let inserts = &alignment.member_inserts[m_idx];
        let mut idxs: Vec<usize> = Vec::new();
        for &slot in &cm[lo..=hi] {
            if let Some(j) = slot {
                idxs.push(j);
            }
        }
        for (_key, ins) in inserts.range(lo..=hi) {
            idxs.extend(ins.iter().copied());
        }
        if idxs.is_empty() {
            // A gap / cost-skipped member has no callee to compare — no opinion.
            continue;
        }
        if idxs.len() != 1 {
            return false;
        }
        let span = &member.node_spans[idxs[0]];
        if !span.is_leaf {
            return false;
        }
        let Some(moniker) = member.callee_monikers.get(&(span.start_byte, span.end_byte)) else {
            return false;
        };
        match seen {
            None => seen = Some(moniker),
            Some(prev) if prev == moniker => {},
            Some(_) => return false,
        }
    }
    seen.is_some()
}

/// `true` when the run's per-member values (the call/method subtree source) differ across members —
/// i.e. the callee/argument structure is not identical. Conservative: any two distinct non-empty
/// values trip the guard.
fn run_callees_differ(per_member_values: &[String]) -> bool {
    let mut seen: Option<&str> = None;
    for v in per_member_values {
        // Skip the gap sentinel: an empty value is "no contribution" (a true indel gap on an
        // aligned member, or a cost-skipped member's "unknown"), NOT a distinct callee. Without
        // this, a skipped member's `""` reads as a second distinct value and trips the guard
        // spuriously — the same un-gated-member residual as Fix 1, here surfacing through the
        // values rather than `alignment.aligned[m]` (this fn only sees
        // `per_member_values`). Aligned gaps already short-circuit to Gapped in
        // `classify_run` before this call, so skipping empties is a no-op on that path and
        // only suppresses the skipped-member false positive. Matches the doc: "any two
        // distinct NON-EMPTY values trip the guard."
        if v.is_empty() {
            continue;
        }
        match seen {
            None => seen = Some(v.as_str()),
            Some(s) if s == v.as_str() => {},
            Some(_) => return true,
        }
    }
    false
}
