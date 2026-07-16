/// AGGREGATE cap on the total number of LCS-DP cells the anti-unify TEMPLATE lane will compute
/// across one class's whole template computation — the parent star-align PLUS every
/// matched-statement re-descent ([`emit_matched_statement_redescent`]).
///
/// This is the template lane's sibling of [`align::LCS_AGGREGATE_CELLS_BUDGET`] (the FIDELITY
/// lane's budget in `class_lcs_ratio`). The two lanes are SEPARATE: `class_lcs_ratio` runs an
/// all-pairs (N²/2) DP, while [`align_to_anchor`] runs a medoid-anchored STAR align (N−1 DPs) plus,
/// since the Fix-2 re-descent, a fresh star align per matched statement — so the fidelity budget
/// never bounded this lane. Until this constant landed, [`align_to_anchor`] had ONLY the per-member
/// [`align::LCS_MAX_SEQ_TOKENS`] guard: a 50-member class of ~2000-token members dominated by one
/// huge matched statement could spend ~20 s+/class on the cold `find_clones` path (measured +7.14 s
/// for a single 40-member/1747-token re-descent), enough to blow an MCP timeout across the 50-class
/// refine budget.
///
/// Same value as the fidelity budget (100M cells) and for the same reason: the per-cell cost is
/// memory-traffic-bound (~10 ns/cell on this box — the `(n+1)·(m+1)` `usize` table thrashes cache),
/// so 100M cells bounds the exact-DP portion of one class's whole template computation to ≈ 1 s.
/// A running cell counter ([`CellBudget`]) is threaded through `align_to_anchor` AND the re-descent
/// so the entire per-class anti-unify draws from ONE budget; once it is exhausted, remaining
/// members (and remaining matched-statement re-descents) are treated like the per-member skip path
/// — all-gap `col_map`, `aligned[m] = false`, and `sampled = true` — so a budget-degraded class is
/// never reported as exact (the `sampled → lcs_sampled → metrics_sampled` chain stays honest).
pub(crate) const ALIGN_AGGREGATE_CELLS_BUDGET: u64 = 100_000_000;

/// Shared running cell counter that bounds the anti-unify template lane's exact `lcs_align` work
/// across one class's WHOLE template computation (the parent star-align + every matched-statement
/// re-descent). Threaded by `&mut` so the parent [`align_to_anchor`] and each
/// [`emit_matched_statement_redescent`] recursion draw from the SAME budget.
///
/// `charge` is called with `|anchor.seq| · |member.seq|` BEFORE an exact `lcs_align`; once the
/// cumulative charge exceeds the budget, `exhausted` latches and every subsequent member /
/// statement takes the skip-and-sample path instead of running exact DP. The cutover is consumed in
/// the existing deterministic member/statement order, so the truncation point — and therefore the
/// whole degraded output — is byte-identical for a given class.
pub(super) struct CellBudget {
    /// Cumulative `Σ |a|·|b|` charged over the exact `lcs_align` calls run so far.
    pub(super) spent: u64,
    /// The cap; `spent > budget` latches `exhausted`.
    pub(super) budget: u64,
    /// `true` once `spent` has exceeded `budget`. Latches — never resets within a class.
    pub(super) exhausted: bool,
}

impl CellBudget {
    pub(super) fn new(budget: u64) -> Self {
        CellBudget { spent: 0, budget, exhausted: false }
    }

    /// Charge `cells` against the budget BEFORE running the exact DP, then return whether the
    /// budget is now exhausted. A pair already charged still runs exactly (the bound is "budget
    /// + one pair"), mirroring [`align::class_lcs_ratio`]'s check-after-charge discipline.
    pub(super) fn charge(&mut self, cells: u64) -> bool {
        self.spent = self.spent.saturating_add(cells);
        if self.spent > self.budget {
            self.exhausted = true;
        }
        self.exhausted
    }
}
