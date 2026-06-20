//! LCS alignment of normalized token sequences (#215 Plan 4a Task 3).
//!
//! # Tie-break rule (documented)
//!
//! The DP backtrace breaks ties deterministically: when `dp[i-1][j] == dp[i][j-1]`, we prefer
//! **`DelA` (decrement `i`)** over `InsB` (decrement `j`). This means, in a tie, we attribute the
//! LCS "progress" to advancing through `b` and declare the `a`-token as deleted rather than the
//! `b`-token as inserted. The rule is arbitrary but fixed — it must not change without bumping
//! `alignment_version`.
//!
//! # NiCad-style `lcs_ratio`
//!
//! `2 * LCS(a, b) / (|a| + |b|)` — a Dice coefficient over token sequences, identical to the
//! similarity measure NiCad uses as `1 - dissimilarity`. `class_lcs_ratio` aggregates per-class as
//! the **minimum** over all unordered pairs: a single loose member drags the class ratio down,
//! mirroring Plan-2's min-not-average discipline.

/// One step in an LCS alignment of two token sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlignOp {
    /// `a[i] == b[j]` — matched token, kept in both.
    Match(usize, usize),
    /// `a[i]` only — deletion from `a` (or equivalently, absent from `b`).
    DelA(usize),
    /// `b[j]` only — insertion in `b` (or equivalently, absent from `a`).
    InsB(usize),
}

/// The result of aligning two token sequences via LCS.
pub(crate) struct Alignment {
    /// Length of the longest common subsequence.
    pub(crate) lcs_len: usize,
    /// Edit operations in ascending position order (Match/DelA/InsB interleaved).
    pub(crate) ops: Vec<AlignOp>,
}

/// Classic LCS dynamic program with a deterministic backtrace.
///
/// Tie-break: when `dp[i-1][j] == dp[i][j-1]`, prefer `DelA` (decrement `i`). See module doc.
pub(crate) fn lcs_align(a: &[String], b: &[String]) -> Alignment {
    let n = a.len();
    let m = b.len();

    // Build the DP table: dp[i][j] = LCS length of a[..i] and b[..j].
    // Use a flat Vec<usize> of (n+1)*(m+1) for cache-friendliness.
    let cols = m + 1;
    let mut dp = vec![0usize; (n + 1) * cols];

    for i in 1..=n {
        for j in 1..=m {
            dp[i * cols + j] = if a[i - 1] == b[j - 1] {
                dp[(i - 1) * cols + (j - 1)] + 1
            } else {
                dp[(i - 1) * cols + j].max(dp[i * cols + (j - 1)])
            };
        }
    }

    let lcs_len = dp[n * cols + m];

    // Backtrace from (n, m) → emit ops in reverse, then reverse at end.
    let mut ops: Vec<AlignOp> = Vec::with_capacity(n + m);
    let mut i = n;
    let mut j = m;

    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            // Match.
            ops.push(AlignOp::Match(i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if j == 0 || (i > 0 && dp[(i - 1) * cols + j] >= dp[i * cols + (j - 1)]) {
            // Tie-break: prefer DelA (decrement i) when dp[i-1][j] >= dp[i][j-1].
            // Also taken when j==0 (must consume remaining a tokens as DelA).
            ops.push(AlignOp::DelA(i - 1));
            i -= 1;
        } else {
            // InsB.
            ops.push(AlignOp::InsB(j - 1));
            j -= 1;
        }
    }

    ops.reverse();
    Alignment { lcs_len, ops }
}

/// Cap on the number of members used in the LCS all-pairs loop. With n members, pairs = n*(n-1)/2;
/// at 64 members that is 2016 pairs, which is a manageable upper bound. When the seqs slice has
/// more than LCS_MEMBER_SAMPLE members, only the first LCS_MEMBER_SAMPLE are used (the slice is
/// already in canonical sorted order, so the sample is deterministic). The caller sees
/// `lcs_sampled = true` when this cap engages.
pub(crate) const LCS_MEMBER_SAMPLE: usize = 64;

/// Cap on the per-pair DP table dimension. When max(|a|, |b|) exceeds this, the pair's LCS
/// ratio is replaced by a token-multiset Dice coefficient (2·|a∩b| / (|a|+|b|)) — a cheap
/// O(n) proxy that avoids allocating the (n+1)*(m+1) usize DP table. The Dice proxy is a
/// reasonable approximation: it equals the exact LCS ratio when sequences are identical or
/// disjoint, and is within the same order of magnitude otherwise. The caller sees
/// `lcs_sampled = true` when this cap engages.
pub(crate) const LCS_MAX_SEQ_TOKENS: usize = 2000;

/// Order-blind upper bound, conservatively clamped. The [`multiset_dice`] proxy used past
/// [`LCS_MAX_SEQ_TOKENS`] is a token-BAG overlap: it ignores token ORDER, so two long functions
/// with the same normalized token multiset but very different ordering score ~1.0 even though their
/// true (order-sensitive) LCS ratio is far lower. Dice is therefore an UPPER BOUND on the LCS
/// ratio, never a lower one. We clamp a proxied pair's ratio to this ceiling — set just BELOW the
/// `confidence_v1` High threshold of 0.9 — so a class whose fidelity rests on the order-blind proxy
/// can reach at most Medium confidence and never full refactorability on the strength of a metric
/// that can't see ordering.
const DICE_PROXY_CEILING: f64 = 0.85;

/// Token-multiset Dice coefficient: `2·|a∩b| / (|a|+|b|)`. Used as a proxy for the LCS ratio
/// when either sequence exceeds [`LCS_MAX_SEQ_TOKENS`] tokens — avoids the O(n·m) DP table
/// for very long sequences at the cost of exactness. O(n+m) via sorted-merge.
///
/// Returns `1.0` when both are empty, `0.0` when one is empty and the other is not.
fn multiset_dice(a: &[String], b: &[String]) -> f64 {
    let denom = (a.len() + b.len()) as f64;
    if denom == 0.0 {
        return 1.0;
    }
    // Build sorted vecs (clone to sort — inputs are bounded by LCS_MAX_SEQ_TOKENS≈2000, so the O(n
    // log n) sort is cheap).
    let mut sa = a.to_vec();
    let mut sb = b.to_vec();
    sa.sort_unstable();
    sb.sort_unstable();
    // Two-pointer multiset intersection count.
    let (mut ia, mut ib, mut intersection) = (0usize, 0usize, 0usize);
    while ia < sa.len() && ib < sb.len() {
        match sa[ia].cmp(&sb[ib]) {
            std::cmp::Ordering::Less => ia += 1,
            std::cmp::Ordering::Greater => ib += 1,
            std::cmp::Ordering::Equal => {
                intersection += 1;
                ia += 1;
                ib += 1;
            },
        }
    }
    2.0 * intersection as f64 / denom
}

/// NiCad-style class fidelity: the **minimum** over all unordered member pairs of
/// `2 * LCS(a, b) / (|a| + |b|)`.
///
/// Returns `(1.0, false)` for a slice with fewer than 2 members (degenerate case) and for any pair
/// of identical sequences. Returns `0.0` when one sequence is empty and the other is not (edge
/// case: `2*0 / (0 + m) == 0`). Guards `|a| + |b| == 0` (both empty) → `1.0`.
///
/// The returned `bool` is `lcs_sampled` — `true` when either cost cap engaged: the member-count cap
/// ([`LCS_MEMBER_SAMPLE`], only the first N members entered the all-pairs loop) or the per-pair
/// length cap ([`LCS_MAX_SEQ_TOKENS`], a pair fell back to the [`multiset_dice`] proxy instead of
/// the exact O(n·m) DP). The caller folds this into the class's `metrics_sampled` flag so a
/// cost-bounded fidelity is distinguishable from an exact one.
pub(crate) fn class_lcs_ratio(seqs: &[Vec<String>]) -> (f64, bool) {
    if seqs.len() < 2 {
        return (1.0, false);
    }

    // Member-count cap: use at most LCS_MEMBER_SAMPLE members. The slice is already in canonical
    // sorted order (by struct_hash then symbol_id), so the first LCS_MEMBER_SAMPLE is a
    // deterministic, reproducible sample.
    let sampled_members = seqs.len() > LCS_MEMBER_SAMPLE;
    let effective = if sampled_members { &seqs[..LCS_MEMBER_SAMPLE] } else { seqs };

    let mut min_ratio = f64::INFINITY;
    let mut sampled_seq = false;

    for i in 0..effective.len() {
        for j in (i + 1)..effective.len() {
            let a = &effective[i];
            let b = &effective[j];
            let denom = (a.len() + b.len()) as f64;
            let ratio = if denom == 0.0 {
                1.0
            } else if a.len().max(b.len()) > LCS_MAX_SEQ_TOKENS {
                // Per-pair length cap: use the Dice proxy instead of the O(n·m) DP. Dice ignores
                // token order so it is an UPPER BOUND on the true LCS ratio — clamp to
                // [`DICE_PROXY_CEILING`] (< the High-confidence threshold) so an order-blind
                // proxy can't earn a class High confidence / full refactorability.
                sampled_seq = true;
                multiset_dice(a, b).min(DICE_PROXY_CEILING)
            } else {
                let lcs = lcs_align(a, b).lcs_len;
                2.0 * lcs as f64 / denom
            };
            if ratio < min_ratio {
                min_ratio = ratio;
            }
        }
    }

    let lcs_sampled = sampled_members || sampled_seq;
    // SAFETY: the `seqs.len() < 2` early return guarantees `effective.len() >= 2`, so the inner
    // loop body (i=0, j=1) executes at least once and `min_ratio` is updated from `f64::INFINITY`.
    // The `f64::INFINITY` fallback is therefore unreachable — assert it in debug, return min_ratio.
    debug_assert!(min_ratio.is_finite(), "min_ratio must be set: effective.len() >= 2");
    (min_ratio, lcs_sampled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| s.to_string()).collect()
    }

    /// Two identical sequences → all Match ops, lcs_len == len, class_lcs_ratio == 1.0.
    #[test]
    fn lcs_align_identical_is_all_match() {
        let a = strs(&["fn", "foo", "(", ")", "{", "x", "}"]);
        let b = a.clone();
        let aln = lcs_align(&a, &b);

        assert_eq!(aln.lcs_len, a.len());
        assert_eq!(aln.ops.len(), a.len());
        for (k, op) in aln.ops.iter().enumerate() {
            assert_eq!(*op, AlignOp::Match(k, k), "op at position {k} was not Match");
        }

        let (ratio, sampled) = class_lcs_ratio(&[a, b]);
        assert!((ratio - 1.0).abs() < 1e-12, "expected ratio 1.0 for identical seqs, got {ratio}");
        assert!(!sampled, "small identical seqs must not engage any cost cap");
    }

    /// b = a with a contiguous run of K extra tokens inserted in the middle.
    /// Expected: the ops contain exactly K contiguous InsB ops; the rest are all Match;
    /// lcs_len == a.len(); ratio == 2*|a| / (|a| + |a|+K).
    #[test]
    fn lcs_align_inserted_token_run() {
        let base = strs(&["fn", "foo", "(", ")", "{", "x", "}"]);
        let insert = strs(&["let", "y", "=", "1"]);
        let k = insert.len(); // 4
        let n = base.len(); // 7

        // Insert the run after index 4 (after "{").
        let mut b = base[..5].to_vec();
        b.extend_from_slice(&insert);
        b.extend_from_slice(&base[5..]);

        let aln = lcs_align(&base, &b);

        assert_eq!(aln.lcs_len, n, "lcs_len should equal |a| == {n}");

        // Count InsB ops — should be exactly K.
        let ins_b_count = aln.ops.iter().filter(|op| matches!(op, AlignOp::InsB(_))).count();
        assert_eq!(ins_b_count, k, "expected {k} InsB ops, got {ins_b_count}");

        // All non-InsB ops should be Match.
        let non_match = aln
            .ops
            .iter()
            .filter(|op| !matches!(op, AlignOp::InsB(_) | AlignOp::Match(_, _)))
            .count();
        assert_eq!(non_match, 0, "unexpected non-Match non-InsB ops");

        // Verify the InsB ops are contiguous in the output.
        let ins_positions: Vec<usize> = aln
            .ops
            .iter()
            .enumerate()
            .filter_map(|(idx, op)| if matches!(op, AlignOp::InsB(_)) { Some(idx) } else { None })
            .collect();
        let first = ins_positions[0];
        let last = ins_positions[ins_positions.len() - 1];
        assert_eq!(
            last - first + 1,
            k,
            "InsB ops are not contiguous: positions {:?}",
            ins_positions
        );

        // Exact ratio check.
        let expected = 2.0 * n as f64 / (n + n + k) as f64;
        let (ratio, _sampled) = class_lcs_ratio(&[base, b]);
        assert!((ratio - expected).abs() < 1e-12, "ratio: expected {expected}, got {ratio}");
    }

    /// a and b differ at exactly one position (one token renamed).
    /// Expected: lcs_len == len-1; ratio < 1.0; the differing position appears as a DelA+InsB pair.
    #[test]
    fn lcs_align_renamed_tokens_differ() {
        let a = strs(&["fn", "foo", "(", "x", ")"]);
        // rename "foo" → "bar"
        let b = strs(&["fn", "bar", "(", "x", ")"]);
        let n = a.len();

        let aln = lcs_align(&a, &b);

        assert_eq!(aln.lcs_len, n - 1, "lcs_len should be {} (one token differs)", n - 1);

        // There must be exactly one DelA and one InsB.
        let del_count = aln.ops.iter().filter(|op| matches!(op, AlignOp::DelA(_))).count();
        let ins_count = aln.ops.iter().filter(|op| matches!(op, AlignOp::InsB(_))).count();
        assert_eq!(del_count, 1, "expected 1 DelA, got {del_count}");
        assert_eq!(ins_count, 1, "expected 1 InsB, got {ins_count}");

        let (ratio, _sampled) = class_lcs_ratio(&[a, b]);
        assert!(ratio < 1.0, "ratio should be < 1.0 for a renamed token, got {ratio}");
        // exact: 2*(n-1)/(n+n) = (n-1)/n
        let expected = (n - 1) as f64 / n as f64;
        assert!((ratio - expected).abs() < 1e-12, "ratio: expected {expected}, got {ratio}");
    }

    /// 3 seqs: two identical (pair ratio 1.0) + one distant (ratio ~0.5).
    /// class_lcs_ratio must return the MIN (~0.5), not the average.
    #[test]
    fn class_lcs_ratio_is_minimum_not_average() {
        // a and b are identical (ratio 1.0 between them).
        let a = strs(&["fn", "foo", "(", ")", "{"]);
        let b = a.clone();

        // c shares only one token with a/b → ratio near 2*1/(5+5) = 0.2 or 2*k/(5+len_c).
        // Use a 5-token sequence that shares exactly 1 token with a.
        let c = strs(&["let", "x", "=", "y", "{"]);
        // only "{" is in common with a → lcs(a,c)=1, ratio = 2*1/(5+5) = 0.2
        let aln_ac = lcs_align(&a, &c);
        let ratio_ac = 2.0 * aln_ac.lcs_len as f64 / (a.len() + c.len()) as f64;
        // ratio_ac should be < 0.5; ensure it's noticeably less than the average of (1.0 + 1.0 +
        // ratio_ac)/3.

        let (class_ratio, _sampled) = class_lcs_ratio(&[a, b, c]);
        assert!(
            (class_ratio - ratio_ac).abs() < 1e-12,
            "class_lcs_ratio ({class_ratio}) should equal the minimum pairwise ratio ({ratio_ac})"
        );
        // Confirm it is indeed the minimum, not the average.
        let average = (1.0 + ratio_ac + ratio_ac) / 3.0;
        assert!(
            class_ratio < average,
            "class_lcs_ratio ({class_ratio}) should be less than average ({average})"
        );
    }

    /// Tie-break pin: a="x y", b="y x". The DP has a classic tie; assert the exact ops produced
    /// by the documented rule (prefer DelA when dp[i-1][j] >= dp[i][j-1]).
    #[test]
    fn lcs_align_deterministic_tiebreak() {
        // a = ["x", "y"], b = ["y", "x"]
        // LCS = 1 (either "x" or "y" can be the LCS — the tie-break picks one).
        // DP table (0-indexed rows=a, cols=b):
        //       ""  "y"  "x"
        //   ""   0    0    0
        //  "x"   0    0    1
        //  "y"   0    1    1
        // Backtrace from (2,2): dp[2][2]=1
        //   a[1]="y" == b[1]="x"? No.
        //   dp[1][2]=1, dp[2][1]=1 → tie → prefer DelA (i=2→1): emit DelA(1), i=1,j=2.
        //   a[0]="x" == b[1]="x"? Yes → Match(0,1), i=0,j=1.
        //   j=1>0, i=0: must InsB. emit InsB(0), j=0.
        //   Reversed: [InsB(0), Match(0,1), DelA(1)]
        let a = strs(&["x", "y"]);
        let b = strs(&["y", "x"]);
        let aln = lcs_align(&a, &b);

        assert_eq!(aln.lcs_len, 1);
        assert_eq!(
            aln.ops,
            vec![AlignOp::InsB(0), AlignOp::Match(0, 1), AlignOp::DelA(1)],
            "deterministic tie-break produced unexpected ops: {:?}",
            aln.ops
        );
    }

    /// Both sequences empty → ratio 1.0, no panic. One empty → ratio 0.0.
    #[test]
    fn lcs_empty_sequences() {
        let empty: Vec<String> = vec![];
        let nonempty = strs(&["fn", "foo"]);

        // Both empty → 1.0.
        let (ratio_both, _) = class_lcs_ratio(&[empty.clone(), empty.clone()]);
        assert!((ratio_both - 1.0).abs() < 1e-12, "both empty: expected 1.0, got {ratio_both}");

        // One empty → 0.0.
        let (ratio_one, _) = class_lcs_ratio(&[empty.clone(), nonempty.clone()]);
        assert!(ratio_one.abs() < 1e-12, "one empty: expected 0.0, got {ratio_one}");

        // Verify lcs_align itself doesn't panic.
        let aln_both = lcs_align(&empty, &empty);
        assert_eq!(aln_both.lcs_len, 0);
        assert!(aln_both.ops.is_empty());

        let aln_one = lcs_align(&empty, &nonempty);
        assert_eq!(aln_one.lcs_len, 0);
        let aln_one_rev = lcs_align(&nonempty, &empty);
        assert_eq!(aln_one_rev.lcs_len, 0);
    }

    /// Fix 1 (#215 Plan 4a): the member-count cap. A class with more than [`LCS_MEMBER_SAMPLE`]
    /// members uses only the first `LCS_MEMBER_SAMPLE` in the all-pairs loop and reports
    /// `lcs_sampled = true`. The ratio is still computed over the (identical) sample, so it stays
    /// `1.0`.
    #[test]
    fn class_lcs_ratio_caps_member_count_and_sets_sampled() {
        // Build LCS_MEMBER_SAMPLE + 1 identical sequences → member-count cap kicks in.
        let seq: Vec<String> = (0..10).map(|i| format!("tok{i}")).collect();
        let seqs: Vec<Vec<String>> = (0..=LCS_MEMBER_SAMPLE).map(|_| seq.clone()).collect();
        assert!(seqs.len() > LCS_MEMBER_SAMPLE);
        let (ratio, sampled) = class_lcs_ratio(&seqs);
        assert!((ratio - 1.0).abs() < 1e-12, "identical seqs → ratio 1.0, got {ratio}");
        assert!(sampled, "member-count cap must set lcs_sampled=true");
    }

    /// Fix 1 (#215 Plan 4a): the per-pair length cap. A pair whose longer sequence exceeds
    /// [`LCS_MAX_SEQ_TOKENS`] tokens skips the O(n·m) DP and uses the [`multiset_dice`] proxy,
    /// reporting `lcs_sampled = true`. Even for two IDENTICAL long sequences (Dice = 1.0) the ratio
    /// is clamped to [`DICE_PROXY_CEILING`] — the proxy is order-blind, so it can never certify a
    /// full ratio.
    #[test]
    fn class_lcs_ratio_caps_long_seq_and_uses_dice_proxy() {
        // Two sequences each LCS_MAX_SEQ_TOKENS + 1 tokens long → per-pair length cap kicks in.
        let long_seq: Vec<String> = (0..=LCS_MAX_SEQ_TOKENS).map(|i| format!("t{i}")).collect();
        let seqs = vec![long_seq.clone(), long_seq.clone()];
        let (ratio, sampled) = class_lcs_ratio(&seqs);
        // Identical long sequences → raw Dice proxy = 1.0, but clamped to the sub-perfect ceiling.
        assert!(
            (ratio - DICE_PROXY_CEILING).abs() < 1e-12,
            "identical long seqs → Dice proxy clamped to {DICE_PROXY_CEILING}, got {ratio}"
        );
        assert!(sampled, "per-pair length cap must set lcs_sampled=true");
    }

    /// Fix 1 round-2 (#215 Plan 4a): the order-blind Dice proxy must NOT certify a perfect ratio.
    /// Two long sequences with the SAME token multiset but REVERSED order have a true LCS ratio far
    /// below 1.0, yet token-bag Dice scores them ~1.0. `class_lcs_ratio` clamps the proxied pair to
    /// [`DICE_PROXY_CEILING`], reports `lcs_sampled = true`, and the resulting ratio bands at most
    /// Medium confidence — never High.
    #[test]
    fn class_lcs_ratio_clamps_reversed_order_long_seqs_below_high() {
        // Build seqs > LCS_MAX_SEQ_TOKENS by repeating a small alphabet, so the multiset is
        // identical but the order is reversed.
        let alphabet = ["a", "b", "c", "d", "e"];
        let forward: Vec<String> =
            (0..=LCS_MAX_SEQ_TOKENS).map(|i| alphabet[i % alphabet.len()].to_string()).collect();
        assert!(forward.len() > LCS_MAX_SEQ_TOKENS, "must exceed the per-pair length cap");
        let reversed: Vec<String> = forward.iter().rev().cloned().collect();
        // Same multiset (a reversal preserves the bag), different order.
        {
            let mut fa = forward.clone();
            let mut rb = reversed.clone();
            fa.sort_unstable();
            rb.sort_unstable();
            assert_eq!(fa, rb, "reversed seq must have the SAME token multiset");
        }

        let (ratio, sampled) = class_lcs_ratio(&[forward, reversed]);
        assert!(sampled, "per-pair length cap must set lcs_sampled=true");
        assert!(
            ratio <= DICE_PROXY_CEILING,
            "order-blind proxy must clamp to ≤ {DICE_PROXY_CEILING}, not ~1.0; got {ratio}"
        );
        // Even with a perfect pairwise-similarity floor the clamped ratio cannot reach High.
        assert_ne!(
            super::super::score::confidence_v1(ratio, 1.0),
            super::super::score::Confidence::High,
            "a Dice-proxied ratio must not band High confidence (ratio {ratio})"
        );
    }

    /// Fix 1 (#215 Plan 4a): the [`multiset_dice`] proxy itself. Identical → 1.0, disjoint → 0.0,
    /// half-overlap → `2·|a∩b| / (|a|+|b|)`.
    #[test]
    fn multiset_dice_basic() {
        // Identical → 1.0.
        let a: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        assert!((multiset_dice(&a, &a) - 1.0).abs() < 1e-12);
        // Disjoint → 0.0.
        let b: Vec<String> = vec!["x".into(), "y".into()];
        assert!(multiset_dice(&a, &b).abs() < 1e-12);
        // Half overlap: a=[a,b,c], b=[b,c,d] → intersection=2, denom=6 → 4/6 ≈ 0.667.
        let c: Vec<String> = vec!["b".into(), "c".into(), "d".into()];
        let expected = 4.0 / 6.0;
        assert!((multiset_dice(&a, &c) - expected).abs() < 1e-9, "got {}", multiset_dice(&a, &c));
    }
}
