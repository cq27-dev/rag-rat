use std::collections::BTreeMap;

use super::super::RefineMember;
use super::super::align::{self, AlignOp, lcs_align};
use super::budget::{ALIGN_AGGREGATE_CELLS_BUDGET, CellBudget};
use super::types::ClassAlignment;

/// Resolve the anchor's position within the canonical-sorted `members` slice.
///
/// When the class threads a medoid `symbol_id`, the anchor is that member (it keeps
/// `body_token_len_medoid` / `similarity_medoid_min` describing the same anchor — see Plan 4b
/// §1.1). Falls back to `0` (the canonical-first `(struct_hash, path, start_byte)` member) when no
/// medoid is threaded or it is not present in the slice.
pub(crate) fn resolve_anchor_idx(members: &[RefineMember], medoid_symbol_id: Option<i64>) -> usize {
    let Some(medoid) = medoid_symbol_id else { return 0 };
    members.iter().position(|m| m.symbol_id == medoid).unwrap_or(0)
}

/// Star-align every non-anchor member to the anchor's spine.
///
/// `N−1` LCS aligns (not `N²`). Cost guards bound the work so a cold refine of a very large clone
/// can never allocate the `(n+1)·(m+1)` LCS DP table unbounded — mirroring
/// [`align::class_lcs_ratio`]'s caps (the anti-unify path used to call exact `lcs_align` on EVERY
/// member with no guard, the OOM bug):
/// - **Degraded anchor** — when the ANCHOR seq exceeds [`align::LCS_MAX_SEQ_TOKENS`], the template
///   can't be computed bounded at all, so EVERY non-anchor member is skipped (all-gap map). The
///   result is an empty-variation-point template over the anchor source (conservative coverage 1.0)
///   with `sampled = true` — no exact `lcs_align` is run.
/// - **Skipped member** — a non-anchor member whose seq exceeds the cap is skipped (all-gap map,
///   `aligned[m] = false`) and `sampled` is set, so it never enters an exact pairwise DP. It reads
///   as a `""` gap at its ordinal but is excluded from fixedness/indel reasoning.
/// - **Aggregate cell budget** — the per-member length cap bounds a SINGLE pair but not the `Σ
///   |anchor|·|member|` PRODUCT across all members (the lane analogue of the fidelity lane's
///   ADVERSARY-B cliff). Once the running cell charge exceeds [`ALIGN_AGGREGATE_CELLS_BUDGET`],
///   every REMAINING member takes the same skip-and-sample path — so no class runs unbounded exact
///   DP regardless of member count or per-member length-under-cap. The budget is SHARED with the
///   matched-statement re-descent (see [`anti_unify`] / [`emit_matched_statement_redescent`]) so
///   the whole per-class anti-unify is bounded by ONE budget.
///
/// Also defensively caps the align pass at [`align::LCS_MEMBER_SAMPLE`] (the loader already returns
/// ≤ `MAX_MEMBERS=50`, so this never engages in practice — it bounds a pathological caller).
pub(crate) fn align_to_anchor(members: &[RefineMember], anchor_idx: usize) -> ClassAlignment {
    let mut budget = CellBudget::new(ALIGN_AGGREGATE_CELLS_BUDGET);
    align_to_anchor_with_budget(members, anchor_idx, &mut budget)
}

/// [`align_to_anchor`] drawing from a CALLER-OWNED [`CellBudget`]. Production passes a fresh budget
/// at [`ALIGN_AGGREGATE_CELLS_BUDGET`] (via the wrapper above); the re-descent passes its PARENT's
/// budget so the parent star-align + every matched-statement re-descent share ONE per-class budget;
/// the aggregate-budget test injects a TINY budget on small seqs to exercise the cutover in
/// milliseconds. The cutover logic is byte-identical at any budget value.
pub(super) fn align_to_anchor_with_budget(
    members: &[RefineMember],
    anchor_idx: usize,
    budget: &mut CellBudget,
) -> ClassAlignment {
    let anchor = &members[anchor_idx];
    let spine_len = anchor.seq.len();
    // The anchor spine itself is too long to align bounded → degrade: skip every non-anchor member.
    // No exact `lcs_align` is ever run, so the huge DP table is never allocated.
    let anchor_too_long = spine_len > align::LCS_MAX_SEQ_TOKENS;

    let mut col_map: Vec<Vec<Option<usize>>> = Vec::with_capacity(members.len());
    let mut member_inserts: Vec<BTreeMap<usize, Vec<usize>>> = Vec::with_capacity(members.len());
    let mut aligned: Vec<bool> = vec![false; members.len()];
    let mut sampled = false;

    for (m_idx, member) in members.iter().enumerate() {
        if m_idx == anchor_idx {
            // Identity: anchor matches itself column-for-column (always "aligned").
            col_map.push((0..spine_len).map(Some).collect());
            member_inserts.push(BTreeMap::new());
            aligned[m_idx] = true;
            continue;
        }
        // Degraded-anchor path: nothing can be aligned bounded → all-gap, mark sampled.
        if anchor_too_long {
            col_map.push(vec![None; spine_len]);
            member_inserts.push(BTreeMap::new());
            sampled = true;
            continue;
        }
        // Defensive member cap (loader already bounds to MAX_MEMBERS): align only the first
        // LCS_MEMBER_SAMPLE members; the rest get an empty (all-gap) map so they read as gaps
        // rather than panic. The slice is canonical-sorted, so the sample is deterministic.
        if m_idx >= align::LCS_MEMBER_SAMPLE {
            col_map.push(vec![None; spine_len]);
            member_inserts.push(BTreeMap::new());
            sampled = true;
            continue;
        }
        // Per-member length guard: a member whose seq exceeds the cap would force the
        // `(spine_len+1)·(member_len+1)` DP allocation — skip it (all-gap, excluded from the
        // aligned set) and mark sampled. No exact `lcs_align` runs on a pair past the cap.
        if member.seq.len() > align::LCS_MAX_SEQ_TOKENS {
            col_map.push(vec![None; spine_len]);
            member_inserts.push(BTreeMap::new());
            sampled = true;
            continue;
        }
        // Aggregate cell budget: charge `|anchor|·|member|` BEFORE the exact DP. Once the running
        // charge exceeds the shared budget, this AND every remaining member take the
        // skip-and-sample path — same all-gap / `aligned[m]=false` / `sampled` machinery as
        // the per-member length skip, so the rest of the pipeline (fixedness / indel /
        // recover) excludes them. A member already charged still runs exactly
        // (check-after-charge → bound is "budget + one pair").
        if budget.charge((spine_len as u64) * (member.seq.len() as u64)) {
            col_map.push(vec![None; spine_len]);
            member_inserts.push(BTreeMap::new());
            sampled = true;
            continue;
        }
        aligned[m_idx] = true;

        let aln = lcs_align(&anchor.seq, &member.seq);
        let mut this_map: Vec<Option<usize>> = vec![None; spine_len];
        let mut inserts: BTreeMap<usize, Vec<usize>> = BTreeMap::new();

        // Walk the ops in diff-blocks. A diff-block is a maximal run of DelA/InsB ops between two
        // Match ops (or the spine edges). Within a block, pair each gapped anchor column (DelA)
        // with an inserted member token (InsB) positionally — that pairing is a
        // SUBSTITUTION, so the member token is keyed at the gap column it fills (not the
        // preceding fixed column). Any surplus InsB tokens (a true insertion, no gap to
        // fill) key at the anchor column the block FOLLOWS. This keeps a clean 1↔1 leaf
        // swap (`DelA(c)+InsB(t)`) from leaking phantom variation onto its fixed `=`/`;`
        // neighbours — the bug the previous keying caused.
        let ops = &aln.ops;
        let mut k = 0usize;
        // The anchor column the current block follows: the last matched/deleted column, -1 at
        // start.
        let mut last_anchor_col: isize = -1;
        // Surplus pure-insertions, resolved AFTER the walk: their containment key depends on the
        // FULL match map (a surplus token may wrap a token matched later in the op stream).
        let mut pending_surplus: Vec<(usize, usize)> = Vec::new(); // (token, follow_key)
        while k < ops.len() {
            match ops[k] {
                AlignOp::Match(i, j) => {
                    this_map[i] = Some(j);
                    last_anchor_col = i as isize;
                    k += 1;
                },
                AlignOp::DelA(_) | AlignOp::InsB(_) => {
                    // Collect the whole diff-block of consecutive DelA/InsB ops.
                    let mut del_cols: Vec<usize> = Vec::new();
                    let mut ins_toks: Vec<usize> = Vec::new();
                    while k < ops.len() {
                        match ops[k] {
                            AlignOp::DelA(i) => {
                                del_cols.push(i);
                                last_anchor_col = i as isize;
                                k += 1;
                            },
                            AlignOp::InsB(j) => {
                                ins_toks.push(j);
                                k += 1;
                            },
                            AlignOp::Match(..) => break,
                        }
                    }
                    // Pair substitutions: ins_toks[p] fills the gap del_cols[p].
                    let paired = del_cols.len().min(ins_toks.len());
                    for p in 0..paired {
                        inserts.entry(del_cols[p]).or_default().push(ins_toks[p]);
                    }
                    // Defer surplus inserts (no gap to fill) — key resolution needs the full map.
                    let follow_key = if last_anchor_col < 0 { 0 } else { last_anchor_col as usize };
                    for &t in &ins_toks[paired..] {
                        pending_surplus.push((t, follow_key));
                    }
                },
            }
        }
        // Resolve surplus inserts against the complete match map. When a surplus member token
        // byte-span CONTAINS a token that matched anchor column `c`, it is wrapping structure for
        // `c`'s subtree (an outer `binary_expression` whose inner expr matched the anchor's flat
        // one) → key it at `c` so the descent attributes it to the right child, not the
        // preceding `=`. A surplus token that contains nothing matched is trailing/leading
        // filler → key at the column the block follows (or 0 when it leads the spine).
        // Sorted by (key, token) for determinism.
        pending_surplus.sort_unstable();
        for (t, follow_key) in pending_surplus {
            let key = anchor_col_for_surplus_insert(member, &this_map, t).unwrap_or(follow_key);
            inserts.entry(key).or_default().push(t);
        }
        for toks in inserts.values_mut() {
            toks.sort_unstable();
        }
        col_map.push(this_map);
        member_inserts.push(inserts);
    }

    ClassAlignment {
        anchor_idx,
        sampled,
        aligned,
        col_map,
        member_inserts,
        spent_cells: budget.spent,
    }
}

/// Attribute a surplus pure-insertion member token `t` to an anchor column by byte-span
/// containment.
///
/// If member token `t`'s byte span strictly contains a member token `j` that matched anchor column
/// `c` (`this_map[c] == Some(j)`), then `t` is wrapping structure for `c`'s subtree — return the
/// anchor column whose matched member token is the WIDEST one `t` contains (the outermost enclosed
/// match — the subtree `t` directly wraps). `None` when `t` contains no matched token (a leaf or
/// trailing filler the caller keys to the column the block follows).
///
/// This is what keeps a member's extra outer node (e.g. the outer `binary_expression` of `a + b +
/// c` whose inner expression matched the anchor's flat `m * n`) attributed to that
/// differing-subtree child instead of the preceding fixed `=` column — preventing phantom variation
/// on that neighbour. Ties on width break to the lowest anchor column for determinism.
fn anchor_col_for_surplus_insert(
    member: &RefineMember,
    this_map: &[Option<usize>],
    t: usize,
) -> Option<usize> {
    let t_span = &member.node_spans[t];
    let mut best: Option<usize> = None;
    let mut best_width = 0usize;
    for (c, &mapped) in this_map.iter().enumerate() {
        let Some(j) = mapped else { continue };
        let js = &member.node_spans[j];
        // `t` strictly contains `j` (a proper byte-span ancestor of the matched token).
        let contains = t_span.start_byte <= js.start_byte
            && js.end_byte <= t_span.end_byte
            && (t_span.start_byte < js.start_byte || js.end_byte < t_span.end_byte);
        if contains {
            let width = js.end_byte.saturating_sub(js.start_byte);
            if best.is_none() || width > best_width {
                best_width = width;
                best = Some(c);
            }
        }
    }
    best
}
