use super::super::RefineMember;
use super::types::ClassAlignment;

/// Yield ONLY the aligned members' values from an ordinal-aligned `per_member_values` slice — the
/// canonical way to iterate the "meaningful" values for any all/any/distinct/type-inference logic.
///
/// `per_member_values[m]` is ordinal-aligned to ALL canonical members (see the INVARIANT on
/// [`recover_values`]); a cost-skipped (un-aligned) member contributes a `""` sentinel that means
/// "value unknown (too long to align)", NOT a genuine value. Routing the all-equal / distinct
/// reductions through this iterator is what keeps a skipped member's `""` from defeating them —
/// the single structural guard against the recurring skipped-member residual. The raw
/// `per_member_values.iter()` over ALL members is reserved for ordinal-aligned output
/// ([`recover_values`] itself and the returned `VariationPoint.per_member_values` field).
pub(super) fn aligned_values<'a>(
    per_member_values: &'a [String],
    alignment: &'a ClassAlignment,
) -> impl Iterator<Item = &'a String> {
    per_member_values
        .iter()
        .enumerate()
        .filter(|&(m, _)| alignment.aligned.get(m).copied().unwrap_or(false))
        .map(|(_, v)| v)
}

/// `true` when every ALIGNED member's value is byte-equal (the C1 all-identical-drop test and its
/// debug_assert). Skipped members' `""` sentinels are ignored (see [`aligned_values`]). There is
/// always ≥1 aligned member (the anchor), so an empty iterator (vacuously `true`) cannot arise for
/// a real alignment; the explicit `first` check documents the contract regardless.
pub(super) fn aligned_values_all_equal(
    per_member_values: &[String],
    alignment: &ClassAlignment,
) -> bool {
    let mut it = aligned_values(per_member_values, alignment);
    let Some(first) = it.next() else { return true };
    it.all(|v| v == first)
}

/// Recover the real source slice each member contributes to the anchor run `[lo..=hi]`.
///
/// # INVARIANT — ordinal alignment + the `""` skipped-member sentinel
///
/// This is the SINGLE source of the `""` sentinel. The returned `Vec` is ordinal-aligned to ALL
/// canonical members (`canonical_member_refs`): index `m` is member `m`'s value, length ==
/// `members.len()`. A cost-skipped (un-aligned) member — one past [`align::LCS_MAX_SEQ_TOKENS`],
/// `alignment.aligned[m] == false` — contributes `""`, which means "value unknown (too long to
/// align)", NOT a genuine empty value. Keeping `""` is REQUIRED: it preserves ordinal alignment so
/// `per_member_values[i]` always maps to canonical member `i` (the MCP / `clones --explain` label
/// contract).
///
/// Consequence — ANY all / any / distinct / type-inference reduction over these values MUST iterate
/// the ALIGNED members only ([`aligned_values`] / [`aligned_values_all_equal`]), NEVER the raw
/// slice. Reducing over the raw slice lets a skipped member's `""` read as a distinct value and
/// silently corrupt the result (the recurring skipped-member residual — historically the C1
/// all-identical drop, `classify_run`'s gapped check, `run_callees_differ`,
/// `uniform_literal_bucket`, `matched_literal_values_differ`, `every_member_single_leaf`,
/// `is_fixed`, `subtree_is_indel`). The raw `per_member_values.iter()` over ALL members is reserved
/// for ordinal-aligned OUTPUT (this function and the returned `VariationPoint.per_member_values`
/// field).
///
/// For member `m`: collect the member token indices that belong to the run — the matched tokens
/// (`col_map[m][i] == Some(j)` for `i ∈ [lo..=hi]`) plus the member-only inserts keyed in
/// `[lo..=hi]` (substitutions key at the gap column they fill, pure insertions at the column they
/// follow — both inside the span when they belong to it). Slice
/// `text.get(span[min].start_byte .. span[max].end_byte)` — the smallest covering source slice. A
/// member that contributes no token at all gaps the run → `""`.
///
/// UTF-8 guard: uses `text.get(..)` (never `&text[..]`); on a `None` slice falls back to the
/// member's baseline token strings joined — never panics.
pub(super) fn recover_values(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    lo: usize,
    hi: usize,
) -> Vec<String> {
    let mut values = Vec::with_capacity(members.len());
    for (m_idx, member) in members.iter().enumerate() {
        let cm = &alignment.col_map[m_idx];
        let inserts = &alignment.member_inserts[m_idx];

        let mut token_idxs: Vec<usize> = Vec::new();
        for &slot in &cm[lo..=hi] {
            if let Some(j) = slot {
                token_idxs.push(j);
            }
        }
        // Inserts keyed in [lo..=hi] belong to this run (the member's own tokens for the span).
        for (_key, idxs) in inserts.range(lo..=hi) {
            token_idxs.extend(idxs.iter().copied());
        }

        if token_idxs.is_empty() {
            // True gap: member contributes nothing to this run.
            values.push(String::new());
            continue;
        }
        let min_j = *token_idxs.iter().min().expect("non-empty");
        let max_j = *token_idxs.iter().max().expect("non-empty");
        let start = member.node_spans[min_j].start_byte;
        let end = member.node_spans[max_j].end_byte;
        let value = match member.text.get(start..end) {
            Some(slice) => slice.to_string(),
            None => {
                // UTF-8 slice miss (should not happen — node spans are char boundaries — but never
                // panic): fall back to the joined baseline token strings.
                (min_j..=max_j).map(|j| member.seq[j].as_str()).collect::<Vec<_>>().join(" ")
            },
        };
        values.push(value);
    }
    values
}
