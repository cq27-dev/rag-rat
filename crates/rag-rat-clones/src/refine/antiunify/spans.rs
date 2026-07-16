use super::super::RefineMember;
use super::types::ClassAlignment;

/// Coalesce the variation columns of `[lo..=hi]` into maximal contiguous runs `(rlo, rhi)`. A
/// column is a variation column iff it is non-fixed for some member OR any member keys an insert at
/// it. Runs are returned column-ascending and non-overlapping.
pub(super) fn variation_runs(
    alignment: &ClassAlignment,
    is_fixed: &[bool],
    lo: usize,
    hi: usize,
) -> Vec<(usize, usize)> {
    let varies = |col: usize| {
        !is_fixed[col] || alignment.member_inserts.iter().any(|ins| ins.get(&col).is_some())
    };
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut col = lo;
    while col <= hi {
        if varies(col) {
            let start = col;
            while col <= hi && varies(col) {
                col += 1;
            }
            runs.push((start, col - 1));
        } else {
            col += 1;
        }
    }
    runs
}

/// `true` iff some member contributes zero tokens to anchor span `[lo..=hi]` while at least one
/// other member contributes ≥1 — a Type-3 indel (the whole subtree is present in some members,
/// absent in others).
pub(super) fn subtree_is_indel(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    lo: usize,
    hi: usize,
) -> bool {
    let mut saw_empty = false;
    let mut saw_filled = false;
    for m_idx in 0..members.len() {
        // A skipped (cost-capped) member is not part of the aligned set — it can't witness a
        // genuine indel, only the all-gap state the guard forced. Exclude it (P1).
        if !alignment.aligned[m_idx] {
            continue;
        }
        if member_token_count(alignment, m_idx, lo, hi) == 0 {
            saw_empty = true;
        } else {
            saw_filled = true;
        }
    }
    saw_empty && saw_filled
}

/// Number of tokens member `m_idx` contributes to anchor span `[lo..=hi]`: matched tokens plus the
/// member-only inserts keyed in `[lo..=hi]`. With substitution-aware keying every insert is keyed
/// at the anchor column it belongs to (a substitution at the gap column it fills, a pure insertion
/// at the column it follows), so the span range `[lo..=hi]` is exactly this member's contribution —
/// no `lo-1` lookback is needed (that fudge belonged to the old "key after the previous column"
/// scheme).
fn member_token_count(alignment: &ClassAlignment, m_idx: usize, lo: usize, hi: usize) -> usize {
    let cm = &alignment.col_map[m_idx];
    let mut count = (lo..=hi).filter(|&i| cm[i].is_some()).count();
    for (_key, idxs) in alignment.member_inserts[m_idx].range(lo..=hi) {
        count += idxs.len();
    }
    count
}

/// `true` iff any member has an insert keyed in `[lo ..= hi]` (member-only tokens inside the span).
pub(super) fn any_member_inserts_within(alignment: &ClassAlignment, lo: usize, hi: usize) -> bool {
    if lo > hi {
        return false;
    }
    alignment.member_inserts.iter().any(|ins| ins.range(lo..=hi).next().is_some())
}

/// The direct children of the anchor node whose subtree is `[lo..=hi]`: the maximal contiguous
/// sub-spans of `[lo+1..=hi]`, each one child subtree (pre-order, byte-span nesting).
pub(super) fn direct_children(anchor: &RefineMember, lo: usize, hi: usize) -> Vec<(usize, usize)> {
    let mut children = Vec::new();
    let mut c = lo + 1;
    while c <= hi {
        let cend = c + subtree_token_count(anchor, c) - 1;
        children.push((c, cend.min(hi)));
        c = cend + 1;
    }
    children
}

/// Number of pre-order tokens in the anchor subtree rooted at column `root` (including the root).
/// Uses the byte spans: in a pre-order walk a subtree is the contiguous run of columns whose byte
/// span is contained in the root's span.
pub(super) fn subtree_token_count(anchor: &RefineMember, root: usize) -> usize {
    let root_end = anchor.node_spans[root].end_byte;
    let mut count = 1;
    let mut k = root + 1;
    while k < anchor.node_spans.len() && anchor.node_spans[k].start_byte < root_end {
        count += 1;
        k += 1;
    }
    count
}
