/// AGGREGATE cap on the total number of LCS-DP cells the anti-unify TEMPLATE lane will compute
/// across one class's whole template computation — the parent star-align PLUS every
/// matched-statement re-descent ([`emit_matched_statement_redescent`]).
///
/// This is the template lane's sibling of [`align::LCS_AGGREGATE_CELLS_BUDGET`] (the FIDELITY
/// lane's budget in `class_fidelity`). The two lanes are SEPARATE: `class_fidelity` runs an
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
/// The latch is `spent > budget`: `spent` only ever grows and nothing charges an exhausted budget,
/// so once a charge crosses the cap every later check sees it exhausted. Two lanes, two
/// disciplines, both owned here:
/// - the fidelity lane ([`align::class_fidelity`]) checks [`Self::is_exhausted`] and then
///   [`Self::charge_and_run`]s the pair — the pair that crosses the cap still runs exactly;
/// - the template lane asks [`Self::reserve`] "may I run this member?" — the member whose charge
///   crosses the cap is skipped, and an exhausted budget is never charged again.
///
/// Either way `spent` exceeds the cap by at most one pair. The cutover is consumed in the existing
/// deterministic member/statement order, so the truncation point — and therefore the whole
/// degraded output — is byte-identical for a given class.
pub(crate) struct CellBudget {
    /// Cumulative `Σ |a|·|b|` charged so far: every exact `lcs_align` run, plus (template lane
    /// only) the one member whose charge crossed the cap.
    spent: u64,
    /// The cap; `spent > budget` means exhausted.
    budget: u64,
}

impl CellBudget {
    pub(crate) fn new(budget: u64) -> Self {
        CellBudget { spent: 0, budget }
    }

    /// A budget at `budget` that has already charged `spent` — exhausted when `spent` is past the
    /// cap, exactly as if the charges had gone through this instance.
    pub(crate) fn resumed(budget: u64, spent: u64) -> Self {
        CellBudget { spent, budget }
    }

    /// A per-class budget drawn from a SHARED CROSS-CLASS allowance: the lane's per-class `cap`,
    /// or less once the `remaining` allowance has drained below it. Hand the spend back with
    /// [`Self::settle`].
    pub(crate) fn draw_from_global(cap: u64, remaining: u64) -> Self {
        Self::new(cap.min(remaining))
    }

    /// Decrement the shared allowance by everything this class charged. `spent` can exceed the
    /// per-class cap by at most one pair, but never the global remaining beyond saturation, so
    /// subsequent classes correctly see a smaller (or zero) allowance.
    pub(crate) fn settle(self, remaining: &mut u64) {
        *remaining = remaining.saturating_sub(self.spent);
    }

    pub(crate) fn spent(&self) -> u64 {
        self.spent
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.spent > self.budget
    }

    /// Charge `cells` for an exact DP the caller runs regardless (it checked
    /// [`Self::is_exhausted`] first). The pair that crosses the cap still runs, so the bound is
    /// "budget + one pair".
    pub(crate) fn charge_and_run(&mut self, cells: u64) {
        self.spent = self.spent.saturating_add(cells);
    }

    /// May the caller run an exact DP of `cells`? An exhausted budget answers `false` WITHOUT
    /// charging, so skipped work never drains the shared allowance. Otherwise `cells` is charged
    /// and the answer is whether the budget still holds — the member whose charge crosses the cap
    /// is charged but skipped.
    pub(crate) fn reserve(&mut self, cells: u64) -> bool {
        if self.is_exhausted() {
            return false;
        }
        self.spent = self.spent.saturating_add(cells);
        !self.is_exhausted()
    }
}
