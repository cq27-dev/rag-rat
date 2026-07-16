use std::collections::BTreeMap;

use super::super::RefineMember;
use super::super::score::Confidence;
use super::alignment::align_to_anchor_with_budget;
use super::budget::{ALIGN_AGGREGATE_CELLS_BUDGET, CellBudget};
use super::classify::{RunClass, classify_run, matched_column_reopen};
use super::render::{coverage_from_mask, render_template};
use super::spans::{
    any_member_inserts_within, direct_children, subtree_is_indel, subtree_token_count,
    variation_runs,
};
use super::statement::emit_block_statement_indel;
use super::types::{
    ClassAlignment, EmittedSpan, MetavarKind, OccSpan, RunMetavar, Template, VariationPoint,
};
use super::values::{aligned_values_all_equal, recover_values};
use super::widen::{
    annotation_type_context, widen_generic_type_head_run, widen_string_content_run,
};

/// Anti-unify the class via a recursive descent of the anchor AST (§1.2–§1.10).
///
/// Rather than coalescing raw LCS variation columns (which straddle when LCS spuriously matches
/// repeated internal-node kinds across structurally-different subtrees), we descend the anchor's
/// pre-order subtree spans and emit one metavar at the tightest subtree where members genuinely
/// diverge. This snaps a differing subtree to ONE metavar (not one per differing leaf), surfaces an
/// indel (a member that gaps a whole subtree) as a `gapped` metavar, and keeps a clean leaf swap as
/// a single-column `value_param`/`type_param`. See the module-level "Simplifications" note.
pub(crate) fn anti_unify(members: &[RefineMember], alignment: &ClassAlignment) -> Template {
    // Production entry: a budget at the lane const, SEEDED with the cells the parent star-align
    // already charged (`alignment.spent_cells`) so the matched-statement re-descent CONTINUES the
    // same per-class budget rather than restarting fresh. The whole per-class anti-unify (parent
    // star-align + every re-descent) is therefore bounded by ONE [`ALIGN_AGGREGATE_CELLS_BUDGET`].
    let mut budget = CellBudget::new(ALIGN_AGGREGATE_CELLS_BUDGET);
    budget.spent = alignment.spent_cells;
    budget.exhausted = budget.spent > budget.budget;
    anti_unify_with_budget(members, alignment, &mut budget)
}

/// Star-align + anti-unify the class drawing the WHOLE template lane (parent star-align + every
/// matched-statement re-descent) from a SHARED CROSS-CLASS allowance (`*remaining_cells`), rather
/// than a fresh per-class [`ALIGN_AGGREGATE_CELLS_BUDGET`]. The per-class cap is
/// `min(ALIGN_AGGREGATE_CELLS_BUDGET, *remaining_cells)`, so the template lane stays bounded per
/// class AND the whole `find_clones` refine pass is bounded by the global allowance. `*remaining`
/// is decremented (`saturating_sub`) by the cells this class's whole anti-unify actually charged.
///
/// Returns the `(ClassAlignment, Template)` pair so the caller persists both. DETERMINISM + the
/// `sampled` honesty chain are preserved exactly as the per-class budget path: the cutover is a
/// pure function of `*remaining` at entry, consumed in deterministic member/statement order, and
/// any budget-degraded member/statement latches `alignment.sampled` / `template.sampled`. With a
/// generous (or `ALIGN_AGGREGATE_CELLS_BUDGET`-sized) allowance the output is byte-identical to the
/// `align_to_anchor` + `anti_unify` pair.
pub(crate) fn anti_unify_global(
    members: &[RefineMember],
    anchor_idx: usize,
    remaining_cells: &mut u64,
) -> (ClassAlignment, Template) {
    let per_class = ALIGN_AGGREGATE_CELLS_BUDGET.min(*remaining_cells);
    let mut budget = CellBudget::new(per_class);
    let alignment = align_to_anchor_with_budget(members, anchor_idx, &mut budget);
    // The same budget rides into the anti-unify: `spent` already carries the star-align charge, so
    // the re-descent continues from where the parent left off (one per-class budget across both
    // sub-lanes), exactly as `anti_unify` seeds itself from `alignment.spent_cells`.
    let template = anti_unify_with_budget(members, &alignment, &mut budget);
    // Decrement the shared allowance by everything this class charged. `spent` can exceed
    // `per_class` by at most "one pair" (charge-then-check), but never the global remaining beyond
    // saturation, so subsequent classes correctly see a smaller (or zero) allowance.
    *remaining_cells = remaining_cells.saturating_sub(budget.spent);
    (alignment, template)
}

/// [`anti_unify`] drawing from a CALLER-OWNED [`CellBudget`]. The budget bounds the exact
/// `lcs_align` work the matched-statement re-descent ([`emit_matched_statement_redescent`]) adds:
/// once it is exhausted, remaining matched statements are left whole-fixed (no further exact DP)
/// and the returned [`Template::sampled`] is set. The re-descent recursion passes the SAME budget
/// down to its `align_to_anchor_with_budget` + `anti_unify_with_budget` calls (the sub-alignment's
/// cells are already charged to it — no re-seed), so a matched statement nested inside another
/// matched statement still draws from the one per-class budget.
pub(super) fn anti_unify_with_budget(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    budget: &mut CellBudget,
) -> Template {
    let anchor = &members[alignment.anchor_idx];
    let spine_len = anchor.seq.len();

    // ── Per-spine-column fixedness (§1.3): FIXED ⟺ every ALIGNED member matched a token there
    // ─────
    // Skipped (un-aligned, cost-capped) members are excluded: their all-gap col_map would otherwise
    // force every column non-fixed. The `sampled` flag is the honest signal that the template was
    // computed over the bounded-aligned subset.
    let mut is_fixed: Vec<bool> = (0..spine_len)
        .map(|i| {
            alignment
                .col_map
                .iter()
                .enumerate()
                .all(|(m, cm)| !alignment.aligned[m] || cm[i].is_some())
        })
        .collect();

    // ── Value-erased matched-column reopen ([`matched_column_reopen`])
    // ──────────────────────────── The baseline normalizer ERASES some leaves' source value to
    // a positional/bucketed token, so two members with DIFFERENT real source produce identical
    // tokens that LCS matches → the column reads FIXED above even though the members genuinely
    // differ. [`matched_column_reopen`] is the ONE place that decides "should this matched
    // column reopen as a variation, and in what role?" (literal / type / callee). A reopened
    // column flows through the normal run→snap→classify→per_member_values→C1 pipeline;
    // `classify_run` then derives the SAME role the reopen decided (literal→value_param,
    // type→type_param, callee→closure_param/differing_callee) from the anchor node kind /
    // callee position, so the reopen decision and the classification stay one source of truth.
    // See [`matched_column_reopen`] for the full position audit.
    for (i, fixed) in is_fixed.iter_mut().enumerate() {
        if *fixed && matched_column_reopen(anchor, i, members, alignment).is_some() {
            *fixed = false;
        }
    }

    // ── Recursive subtree descent → metavar spans (§1.2/§1.4) ────────────────────────────────────
    // `budget` is threaded so the matched-statement re-descent (the only descent path that runs
    // more exact `lcs_align`) draws from the shared per-class budget; `redescent_sampled`
    // latches when the re-descent left a matched statement whole-fixed because the budget was
    // exhausted.
    let mut spans: Vec<EmittedSpan> = Vec::new();
    let mut redescent_sampled = false;
    if spine_len > 0 {
        let root_end = subtree_token_count(anchor, 0) - 1;
        emit_metavar_spans(
            members,
            alignment,
            anchor,
            &is_fixed,
            0,
            root_end,
            budget,
            &mut redescent_sampled,
            &mut spans,
        );
    }
    spans.sort_by_key(EmittedSpan::lo);

    // ── Build a candidate metavar per span (values + classify) ───────────────────────────────────
    // Drop any candidate that is NOT gapped and whose per-member values are all byte-identical: a
    // genuinely fixed region a stray surplus-insert (an LCS tie peeling a member's `(` etc.)
    // promoted to a spurious variation point. A non-gap metavar must have ≥2 distinct values; if
    // all values are equal it's fixed, not a variation point. A gapped run legitimately has mixed
    // `""`/value (so it is never all-equal) and stays. Surviving spans drive coverage, so the
    // dropped columns fall back to fixed (coverage rises) — see the coverage recompute below.
    let mut candidates: Vec<RunMetavar> = Vec::with_capacity(spans.len());
    let mut surviving_spans: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    // Columns of zero-width MEMBER-ONLY inserts (a statement absent from the anchor): they occupy
    // no anchor column, so they must NOT fill the coverage mask and render as an insertion
    // point right after their attach column (without erasing that fixed token). See
    // `EmittedSpan::Statement`.
    let mut zero_width_cols: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for span in &spans {
        // Two Raw-span widenings, both turning a leaf hole that would render an INVALID
        // substitution into a whole-subtree hole:
        // - Fix 3 (Codex round-5): a `string_content` LEAF covers only the text INSIDE the quotes
        //   (`hello`), so the surrounding `"` quotes would render as FIXED text around the hole →
        //   `"⟨m0⟩"` with `arg0: &str` (a `&str` value carries its own quotes). Widen to the
        //   enclosing `string_literal` so the hole is the WHOLE `"hello"`.
        // - Fix 4 (Codex round-7): a `generic_type` HEAD leaf (`Vec` of `Vec<i32>` vs
        //   `Option<i32>`) reopens only the head, so the type args render as FIXED text around the
        //   hole → `⟨m0⟩<i32>` / `-> T0<i32>` (invalid Rust, and hard-codes the anchor's type
        //   args). Widen to the enclosing `generic_type` so the WHOLE `Vec<i32>` is one type_param
        //   → `⟨m0⟩` / generic `T0` with values `[Vec<i32>, Option<i32>]`. An INNER-arg-only diff
        //   (`Vec<i32>`/`Vec<u8>`) reopens the inner leaf (not the head), so it is NOT widened — it
        //   stays the existing inner case.
        // Statement / Classified spans are never bare type/string leaves, so widening only applies
        // to Raw.
        let (lo, hi) = match span {
            EmittedSpan::Raw(rlo, rhi) => {
                let (wlo, whi) = widen_string_content_run(anchor, *rlo, *rhi);
                widen_generic_type_head_run(anchor, wlo, whi)
            },
            EmittedSpan::Statement { lo, hi, .. } | EmittedSpan::Classified { lo, hi, .. } =>
                (*lo, *hi),
        };
        let snapped_kind = anchor.node_spans[lo].kind;
        // A statement-snapped indel carries pre-computed, statement-balanced per_member_values
        // (recovered from clean statement-level structural alignment, NOT the tangled token
        // col_map) and is always Gapped. A Classified span (Fix 2 matched-statement re-descent)
        // carries a pre-classified VP from a clean sub-anti_unify — consumed verbatim, NOT
        // re-recovered from the parent's tangled col_map. A raw run recovers from the col_map and
        // is classified.
        let (per_member_values, run_class, is_zero_width) = match span {
            EmittedSpan::Statement { per_member_values, zero_width, .. } => (
                per_member_values.clone(),
                RunClass {
                    kind: MetavarKind::Gapped,
                    type_hint: None,
                    confidence: Confidence::Low,
                    differing_callee: false,
                },
                *zero_width,
            ),
            EmittedSpan::Classified {
                per_member_values,
                kind,
                type_hint,
                confidence,
                differing_callee,
                zero_width,
                ..
            } => (
                per_member_values.clone(),
                RunClass {
                    kind: *kind,
                    type_hint: type_hint.clone(),
                    confidence: *confidence,
                    differing_callee: *differing_callee,
                },
                *zero_width,
            ),
            EmittedSpan::Raw(..) => {
                let pmv = recover_values(members, alignment, lo, hi);
                let rc = classify_run(members, alignment, anchor, lo, hi, &pmv);
                (pmv, rc, false)
            },
        };
        let RunClass { kind, type_hint, confidence, differing_callee } = run_class;
        // C1 guard: drop a non-gapped, all-identical-value candidate (a fixed region mis-keyed by a
        // stray insert). Compare only ALIGNED members' values (`aligned_values`): a cost-skipped
        // member's `""` sentinel is "value unknown", NOT a distinct value, so it must not defeat
        // the all-equal test and resurrect a spurious metavar over a genuinely-fixed region
        // (the 8th skipped-member residual — see the INVARIANT note on `recover_values`).
        // There is always ≥1 aligned member (the anchor), so the comparison is never empty.
        if kind != MetavarKind::Gapped && aligned_values_all_equal(&per_member_values, alignment) {
            continue;
        }
        if is_zero_width {
            zero_width_cols.insert(lo);
        } else {
            surviving_spans.push((lo, hi));
        }
        candidates.push(RunMetavar {
            lo,
            snapped_kind,
            per_member_values,
            kind,
            type_hint,
            confidence,
            differing_callee,
            // The syntactic type context at this occurrence — the recovered `: T` annotation text
            // (Fix 3, Codex round-7). Part of the collapse key so two occurrences with the SAME
            // value tuple but DIFFERENT annotations (`let a: i32 = 1` / `let b: u8 = 1`) stay
            // SEPARATE metavars; collapsing them would make `propose_signature` reuse one param
            // across two distinct typed slots (an invalid `i32`+`u8` signature).
            type_context: annotation_type_context(anchor, lo),
        });
    }

    // ── Recurrence collapse (§1.7): dedupe spans with identical (values, kind)
    // ────────────────────
    let variation_points = collapse_recurring(candidates);

    // No surviving non-gapped metavar may have all-equal per-member values (the C1 invariant).
    // Same aligned-only filter as the C1 guard above: a skipped member's `""` is excluded so the
    // assertion checks "≥2 distinct values AMONG ALIGNED MEMBERS", matching the guard exactly.
    debug_assert!(
        variation_points.iter().all(|vp| vp.kind == MetavarKind::Gapped
            || !aligned_values_all_equal(&vp.per_member_values, alignment)),
        "a surviving non-gapped metavar must have ≥2 distinct values among aligned members: \
         {variation_points:?}"
    );

    // ── Coverage (§1.10): fixed_spine_columns / total_spine_columns ──────────────────────────────
    // Every column a SURVIVING metavar span covers counts as non-fixed (a subtree-snapped metavar
    // marks its whole span). Dropped (all-identical) spans are fixed, so coverage is recomputed
    // over the surviving spans only — the dropped columns rejoin the fixed mask and coverage rises.
    // Zero-width member-only inserts occupy no anchor column → excluded from the mask.
    let mut snapped_fixed = is_fixed.clone();
    for &(lo, hi) in &surviving_spans {
        snapped_fixed[lo..=hi].fill(false);
    }
    let anti_unify_coverage = coverage_from_mask(&snapped_fixed);

    // ── Template text (§1.9) ─────────────────────────────────────────────────────────────────────
    // occurrences store only the lo column after collapse; map lo → hi so render recovers each
    // span.
    let lo_to_hi: BTreeMap<usize, usize> =
        surviving_spans.iter().map(|&(lo, hi)| (lo, hi)).collect();
    let text = render_template(anchor, &variation_points, &lo_to_hi, &zero_width_cols);

    // Carry each occurrence's REAL snapped span + zero-width flag (Fix 2): a consuming hole keeps
    // its `(lo, hi)` from `surviving_spans`; a zero-width member-only insert is `(lo, lo)` with
    // `zero_width = true`. The matched-statement re-descent translates these verbatim into the
    // parent so a straddling multi-subtree hole is not truncated and a zero-width insert is not
    // turned into a consuming hole.
    let mut occurrence_spans: BTreeMap<usize, OccSpan> = BTreeMap::new();
    for &(lo, hi) in &surviving_spans {
        occurrence_spans.insert(lo, OccSpan { hi, zero_width: false });
    }
    for &lo in &zero_width_cols {
        occurrence_spans.insert(lo, OccSpan { hi: lo, zero_width: true });
    }

    Template {
        text,
        variation_points,
        anti_unify_coverage,
        sampled: redescent_sampled,
        occurrence_spans,
    }
}

/// Recursive anchor-subtree descent that appends metavar spans for the subtree `[lo..=hi]` (a
/// single anchor node's whole pre-order span).
///
/// Decision per node:
/// 1. **Fixed-for-all & insert-free** — every column fixed and no member inserts inside
///    `[lo..=hi]`: emit nothing (verbatim template text).
/// 2. **Indel** — some member contributes zero tokens to `[lo..=hi]` while another contributes
///    some: emit ONE metavar over `[lo..=hi]` (a gapped subtree); don't descend.
/// 3. **Leaf** — emit ONE metavar over the leaf (`[lo..=lo]`).
/// 4. **Header varies** — the node's header column `lo` is itself non-fixed, or a member inserts
///    extra structure right at the node root (keyed at `lo`): the node identity differs across
///    members. Snap ONE metavar over the whole node (`a + b + c` vs `m * n`) — descending would
///    mis-attribute the spuriously-matched header.
/// 5. **Block-statement indel (Type-3 statement snap)** — the node is a statement container
///    (`block` / `declaration_list` / …) and the members differ by a WHOLE inserted/removed
///    statement. The token LCS tangles closing punctuation (`( ) ;` `}`) across the statement
///    boundary, so the raw col_map can't be trusted here: re-derive via clean statement-level
///    structural alignment ([`emit_block_statement_indel`]). Emits whole-statement gapped metavars
///    with balanced, ordinal-correct per-member values and leaves matched statements fixed.
/// 6. **Decomposable** — otherwise coalesce the node's variation into contiguous runs and route
///    each: a run contained in ONE direct child recurses into that child (a clean leaf swap bottoms
///    out at rule 3 / a differing sub-subtree at rule 4); a run that STRADDLES ≥2 children is an
///    LCS-tangled edit — emit it as ONE raw span rather than splitting it across the straddled
///    siblings.
#[allow(clippy::too_many_arguments)] // the descent threads the shared cell budget + sample flag.
fn emit_metavar_spans(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    anchor: &RefineMember,
    is_fixed: &[bool],
    lo: usize,
    hi: usize,
    budget: &mut CellBudget,
    redescent_sampled: &mut bool,
    out: &mut Vec<EmittedSpan>,
) {
    // (1) Fixed-for-all subtree with no interior inserts → fixed text.
    if (lo..=hi).all(|c| is_fixed[c]) && !any_member_inserts_within(alignment, lo, hi) {
        return;
    }

    // (2) Indel: a member gaps the whole subtree while another fills it → one gapped metavar.
    if subtree_is_indel(members, alignment, lo, hi) {
        out.push(EmittedSpan::Raw(lo, hi));
        return;
    }

    // (3) Leaf node → one metavar over the leaf.
    if anchor.node_spans[lo].is_leaf {
        out.push(EmittedSpan::Raw(lo, lo));
        return;
    }

    // (4) Header varies (the node root itself differs / carries extra structure) → snap whole node.
    let header_varies =
        !is_fixed[lo] || alignment.member_inserts.iter().any(|ins| ins.get(&lo).is_some());
    if header_varies {
        out.push(EmittedSpan::Raw(lo, hi));
        return;
    }

    // (5) Block-statement indel: the node holds statement-like children and some member adds/drops
    // a whole statement. The flat token LCS tangles the inserted statement's `( ) ;`/`}` across
    // the statement boundary (the headline Type-3 defect), so the raw col_map is unusable here.
    // Snap to statement boundaries and recover whole-statement values via structural alignment
    // instead.
    if is_statement_container(anchor.node_spans[lo].kind)
        && emit_block_statement_indel(
            members,
            alignment,
            anchor,
            is_fixed,
            lo,
            hi,
            budget,
            redescent_sampled,
            out,
        )
    {
        return;
    }

    // (6) Decomposable: coalesce variation into contiguous runs and route each to a child or emit
    // it.
    let children = direct_children(anchor, lo, hi);
    let runs = variation_runs(alignment, is_fixed, lo + 1, hi);
    let mut recursed_children: Vec<(usize, usize)> = Vec::new();
    for (rlo, rhi) in runs {
        // The direct children this run touches (overlaps).
        let touched: Vec<(usize, usize)> =
            children.iter().copied().filter(|&(clo, chi)| clo <= rhi && rlo <= chi).collect();
        if touched.len() == 1 {
            // Contained in one child → recurse into it (once, even if several runs touch it).
            let child = touched[0];
            if !recursed_children.contains(&child) {
                recursed_children.push(child);
                emit_metavar_spans(
                    members,
                    alignment,
                    anchor,
                    is_fixed,
                    child.0,
                    child.1,
                    budget,
                    redescent_sampled,
                    out,
                );
            }
        } else {
            // Straddles ≥2 children (or no child — defensive) → emit the run as one raw span.
            out.push(EmittedSpan::Raw(rlo, rhi));
        }
    }
}

/// `true` for an anchor node kind that holds a sequence of statement-like children (a block / item
/// list). The statement-snap (rule 5) only engages inside one of these.
fn is_statement_container(kind: &str) -> bool {
    matches!(kind, "block" | "declaration_list" | "field_declaration_list" | "statement_block")
}

/// Collapse runs whose `(per_member_values, snapped_kind, role, differing_callee, type_context)`
/// are byte-identical into a single metavar with multiple `occurrences[]`. Conservative (§1.7):
/// only co-varying reuse of the SAME role AND SAME type context collapses. Assigns `metavar_id`s
/// `m0, m1, …` ascending by first occurrence column (deterministic — `candidates` arrive
/// column-ascending from `coalesce_runs`).
///
/// The collapse key includes:
/// - the classified ROLE ([`MetavarKind`]) and `differing_callee`, not just `(values,
///   snapped_kind)` (P2 / Codex #6). The same source values can recur in DIFFERENT roles — a
///   differing identifier `foo`/`bar` as a call HEAD in one spot (`closure_param`,
///   `differing_callee=true`) and as a plain VALUE in another (`value_param`). Keying on the role
///   keeps the two distinct.
/// - the SYNTACTIC TYPE CONTEXT (the recovered `: T` annotation text, Fix 3 Codex round-7). The
///   same value tuple + role can recur in DIFFERENT typed contexts — `let a: i32 = 1; let b: u8 =
///   1` vs both → `2`: two value_params `[1→2]`, but one is `i32`-annotated and the other `u8`.
///   Collapsing them into one metavar makes `propose_signature` recover the FIRST occurrence's
///   annotation and reuse one param across both typed slots → an invalid `i32`+`u8` signature.
///   Keying on the annotation text keeps occurrences with distinct type contexts SEPARATE.
pub(super) fn collapse_recurring(candidates: Vec<RunMetavar>) -> Vec<VariationPoint> {
    // Group by (per_member_values, snapped_kind, role, differing_callee, type_context), preserving
    // first-seen order via a Vec of groups.
    struct Group {
        key_values: Vec<String>,
        key_kind: &'static str,
        key_type_context: Option<String>,
        occurrences: Vec<usize>,
        kind: MetavarKind,
        type_hint: Option<String>,
        confidence: Confidence,
        differing_callee: bool,
    }
    let mut groups: Vec<Group> = Vec::new();
    for c in candidates {
        let existing = groups.iter_mut().find(|g| {
            g.key_kind == c.snapped_kind
                && g.kind == c.kind
                && g.differing_callee == c.differing_callee
                && g.key_type_context == c.type_context
                && g.key_values == c.per_member_values
        });
        match existing {
            Some(g) => {
                // Co-varying reuse of the same role — record the additional occurrence (the run's
                // lo column). `differing_callee` is part of the key, so it already
                // matches.
                g.occurrences.push(c.lo);
            },
            None => groups.push(Group {
                key_values: c.per_member_values,
                key_kind: c.snapped_kind,
                key_type_context: c.type_context,
                occurrences: vec![c.lo],
                kind: c.kind,
                type_hint: c.type_hint,
                confidence: c.confidence,
                differing_callee: c.differing_callee,
            }),
        }
    }

    // Sort groups by first occurrence column so metavar_ids ascend by spine column
    // deterministically.
    groups.sort_by_key(|g| g.occurrences.iter().copied().min().unwrap_or(0));

    groups
        .into_iter()
        .enumerate()
        .map(|(idx, mut g)| {
            g.occurrences.sort_unstable();
            VariationPoint {
                metavar_id: format!("m{idx}"),
                kind: g.kind,
                occurrences: g.occurrences,
                per_member_values: g.key_values,
                extraction_role: g.kind.as_db_str(),
                type_hint: g.type_hint,
                confidence: g.confidence,
                differing_callee: g.differing_callee,
            }
        })
        .collect()
}
