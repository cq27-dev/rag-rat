//! Anti-unification (least general generalization) of a clone class (#215 Plan 4b Task 5d).
//!
//! Turns a set of structurally-similar members into an extractable-helper **template** plus a list
//! of **variation points** (metavars): the columns where the members agree become fixed source, the
//! columns where they disagree become holes with ordinal-aligned per-member values and an
//! extraction-role classification.
//!
//! # Mechanism — medoid-anchored star LCS
//!
//! Pure algorithm, no DB. One anchor member (the class medoid, or the canonical-first member when
//! no medoid is threaded) carries the spine. Each other member is aligned to the anchor with
//! [`super::align::lcs_align`] (N−1 aligns, not N²). A spine column is FIXED iff *every* member
//! contributes a matched token there; otherwise it is a VARIATION column. Contiguous variation
//! columns coalesce into runs, each run snaps to the tightest enclosing anchor subtree, and the
//! members' real source slices (recovered through the seq↔AST [`NodeSpan`] bijection) become the
//! per-member values.
//!
//! # Baseline-only — the Plan-3 SCIP seam
//!
//! Classification is syntactic. In particular, two call/method heads whose callee subtree DIFFERS
//! across members are **always** `closure_param` Medium/Low — never a high-confidence
//! `value_param`. Baseline normalization cannot prove two differing callees resolve to the same
//! symbol, so the differing-callee guard ([`classify_run`]) deliberately refuses to promote them.
//! SCIP resolution (Plan 3) is what would lift that; until then the conservative band is the honest
//! one.
//!
//! # Determinism
//!
//! Members arrive in canonical `(struct_hash, path, start_byte)` order — the `per_member_values`
//! ordinal basis. `metavar_id`s are assigned `m0, m1, …` ascending by the run's first spine column.
//! No HashMap iteration drives any output ordering (inserts use a `BTreeMap` keyed by anchor
//! column).
//! The same struct_hash multiset therefore always yields the same template.
//!
//! # Simplifications (documented, cosmetic-only)
//!
//! - **Subtree snap** ([`snap_run`]): a run snaps to an anchor subtree only when the anchor node at
//!   the run's first column is itself the subtree root AND its whole pre-order token count fits the
//!   run exactly. Otherwise the run keeps its raw column span and the snapped kind is the anchor's
//!   node kind at `lo`. A looser snap (outermost-contained / smallest-enclosing) is a template
//!   *legibility* refinement, never a correctness one — `per_member_values` are recovered from real
//!   source byte offsets regardless of how the run snaps.
//! - **Recurrence collapse** ([`collapse_recurring`]): two runs collapse into one metavar with
//!   multiple `occurrences[]` only when their `per_member_values` tuple AND snapped-subtree kind
//!   are byte-identical across all members. Conservative — a near-miss stays two metavars.

use std::collections::BTreeMap;

use super::align::{self, AlignOp, lcs_align};
use super::score::Confidence;
use crate::index::clones::normalize::NodeSpan;
use crate::index::clones::refine::RefineMember;

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
struct CellBudget {
    /// Cumulative `Σ |a|·|b|` charged over the exact `lcs_align` calls run so far.
    spent: u64,
    /// The cap; `spent > budget` latches `exhausted`.
    budget: u64,
    /// `true` once `spent` has exceeded `budget`. Latches — never resets within a class.
    exhausted: bool,
}

impl CellBudget {
    fn new(budget: u64) -> Self {
        CellBudget { spent: 0, budget, exhausted: false }
    }

    /// Charge `cells` against the budget BEFORE running the exact DP, then return whether the
    /// budget is now exhausted. A pair already charged still runs exactly (the bound is "budget
    /// + one pair"), mirroring [`align::class_lcs_ratio`]'s check-after-charge discipline.
    fn charge(&mut self, cells: u64) -> bool {
        self.spent = self.spent.saturating_add(cells);
        if self.spent > self.budget {
            self.exhausted = true;
        }
        self.exhausted
    }
}

/// Extraction role for a variation point — what kind of helper parameter the hole would become.
/// Persisted as a stable machine string via [`MetavarKind::as_db_str`] (the `extraction_role` field
/// mirrors it for legibility in the JSON contract).
///
/// `serde` serializes each variant to its `as_db_str` machine string (`value_param`,
/// `closure_param`, `type_param`, `gapped`) so the persisted `variation_points_json` is legible
/// without consulting the Rust enum (matches the `_json` column convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetavarKind {
    /// A single compatible leaf (local identifier or literal) — a plain by-value parameter.
    ValueParam,
    /// A multi-token differing subtree, OR a differing call/method callee head (the Plan-3 SCIP
    /// seam). Would become an `impl Fn` / passed-in operation.
    ClosureParam,
    /// A type position (`type_identifier` / `generic_type` / `scoped_type_identifier` /
    /// `primitive_type`) — would become a generic type parameter.
    TypeParam,
    /// At least one member gaps the run (a Type-3 indel). Not promotable to a clean parameter.
    Gapped,
}

impl MetavarKind {
    /// Stable lower-case machine string. Used both as the persisted role and as `extraction_role`.
    pub(crate) fn as_db_str(&self) -> &'static str {
        match self {
            MetavarKind::ValueParam => "value_param",
            MetavarKind::ClosureParam => "closure_param",
            MetavarKind::TypeParam => "type_param",
            MetavarKind::Gapped => "gapped",
        }
    }
}

/// One variation point in the anti-unified template.
///
/// Serialized as an element of the persisted `variation_points_json` array (Plan-4b Task 7). The
/// `confidence` band serializes via `Confidence`'s snake_case repr (`high`/`medium`/`low`).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct VariationPoint {
    /// `m0`, `m1`, … assigned ascending by the metavar's first spine column.
    pub(crate) metavar_id: String,
    pub(crate) kind: MetavarKind,
    /// Anchor spine column index(es) this metavar occupies. More than one ⟺ a recurrence-collapsed
    /// metavar (the same value reused at several spine positions).
    pub(crate) occurrences: Vec<usize>,
    /// Real source slices recovered per member, ordinal-aligned to the canonical-sorted members.
    /// `""` is the gap sentinel (the member contributes no token to this run).
    pub(crate) per_member_values: Vec<String>,
    /// `== kind.as_db_str()` — one source of truth, persisted alongside the kind for JSON
    /// legibility.
    pub(crate) extraction_role: &'static str,
    /// A literal bucket (e.g. `LIT_INTEGER_LITERAL`) when a `value_param` is a uniform literal
    /// kind.
    pub(crate) type_hint: Option<String>,
    pub(crate) confidence: Confidence,
    /// `true` ONLY when the differing-callee guard actually fired for this metavar — a genuine
    /// differing callee / method-name head (the Plan-3 SCIP seam). Carried EXPLICITLY rather than
    /// re-derived from `(kind, confidence)` downstream: `ClosureParam` is also `Medium` for
    /// generic closure-ish subtrees (`binary_expression`, `field_expression`, …), so inferring
    /// `differing_callee` from the band would wrongly apply the call-resolution downgrade to
    /// non-callee diffs (Fix 5). Read by [`super::score::metavar_profile`]. Persisted in
    /// `variation_points_json` (serde) — `#[serde(default)]` so legacy rows lacking it parse.
    #[serde(default)]
    pub(crate) differing_callee: bool,
}

/// The anti-unified result for a clone class.
#[derive(Debug, Clone)]
pub(crate) struct Template {
    /// Human-readable template rendered from the anchor's real source: fixed runs verbatim,
    /// variation runs as `⟨m0⟩`, gapped runs as `⟨m2?⟩`.
    pub(crate) text: String,
    /// Variation points, `metavar_id`-ascending (== first-spine-column-ascending).
    pub(crate) variation_points: Vec<VariationPoint>,
    /// `fixed_spine_columns / total_spine_columns` ∈ [0,1]. `1.0` when all members are identical.
    pub(crate) anti_unify_coverage: f64,
    /// `true` when the matched-statement re-descent ([`emit_matched_statement_redescent`]) hit the
    /// shared [`CellBudget`] and left one or more matched statements whole-fixed instead of
    /// running their exact sub-DP. The parent [`ClassAlignment::sampled`] only covers the
    /// parent star-align, so the caller folds THIS flag in too (`alignment.sampled ||
    /// template.sampled`) before persisting `lcs_sampled` — a budget-degraded re-descent is
    /// never reported as exact. (`false` for the common under-budget class, so its output is
    /// byte-identical to before.)
    pub(crate) sampled: bool,
    /// Per-occurrence snapped span + zero-width flag, keyed by the occurrence's `lo` anchor column
    /// (`VariationPoint::occurrences` carries only the `lo` columns). The matched-statement
    /// re-descent (Fix 2, Codex round-7) reads this to translate a sub-VP's ACTUAL occurrence span
    /// into the parent — carrying the real `hi` and `zero_width` rather than re-deriving `hi` from
    /// the parent subtree (which truncates a straddling multi-subtree hole and turns a member-only
    /// zero-width insert into a consuming hole). Not serialized — an internal carrier between the
    /// sub-template and its parent-space translation.
    pub(crate) occurrence_spans: BTreeMap<usize, OccSpan>,
}

/// The snapped span (`lo..=hi`) and zero-width flag of one variation-point OCCURRENCE — carried out
/// of [`anti_unify`] on [`Template::occurrence_spans`] so the matched-statement re-descent can
/// translate the sub-VP's real span into the parent column space (Fix 2, Codex round-7). `lo` is
/// the map key; `hi` and `zero_width` are the data the re-descent would otherwise have to
/// (incorrectly) re-derive.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OccSpan {
    pub(crate) hi: usize,
    /// `true` for a MEMBER-ONLY zero-width insert (occupies no anchor column — renders after `lo`
    /// without consuming it). A consuming hole is `false`.
    pub(crate) zero_width: bool,
}

/// Per-member alignment to the spine anchor.
pub(crate) struct ClassAlignment {
    /// Index of the anchor member within the (canonical-sorted) `members` slice.
    pub(crate) anchor_idx: usize,
    /// `true` when a cost guard engaged during alignment (the anchor seq exceeded
    /// [`align::LCS_MAX_SEQ_TOKENS`] → the whole template is degraded, OR at least one non-anchor
    /// member exceeded the cap and was skipped from the alignment). The caller folds this into the
    /// class `lcs_sampled` / `metrics_sampled` flag so a cost-bounded template is distinguishable
    /// from an exact one. Skipped members read as gaps in `per_member_values`; the sampled flag is
    /// the honest signal that those values reflect only the bounded-aligned members.
    pub(crate) sampled: bool,
    /// `true` for a member that actually entered the LCS alignment (anchor + every non-anchor
    /// whose seq fit [`align::LCS_MAX_SEQ_TOKENS`]). A `false` member was skipped (its
    /// `col_map` is all-gap, its `member_inserts` empty) and is EXCLUDED from fixedness /
    /// indel reasoning so it cannot manufacture a spurious whole-spine gap — it still
    /// contributes a `""` gap value at its ordinal so `per_member_values` stays aligned to all
    /// canonical members.
    aligned: Vec<bool>,
    /// `col_map[m][i]` = the token index in member `m`'s seq matched to anchor spine column `i`,
    /// or `None` when member `m` deletes anchor column `i`. `col_map[anchor_idx]` is the
    /// identity map. Outer index is the member ordinal (parallel to `members`).
    col_map: Vec<Vec<Option<usize>>>,
    /// `member_inserts[m]` keys an anchor spine column to the member-only token indices the
    /// alignment associates with it. Two cases, both keyed at the column they BELONG to:
    /// - **Substitution** — an `InsB` token that fills a `DelA` (gapped) anchor column is keyed at
    ///   that gap column. `let x = 10` vs `let x = 2.5` aligns the differing literal as
    ///   `DelA(lit_col) + InsB(lit_tok)`; the member's token keys at `lit_col`, NOT the preceding
    ///   `=`. This is what keeps a clean leaf swap from leaking phantom variation onto its fixed
    ///   neighbour.
    /// - **Pure insertion** — an `InsB` token with no gap column to fill (extra member structure)
    ///   is keyed at the anchor column it FOLLOWS (the last matched/deleted column, or `0` when it
    ///   leads the spine). Trailing inserts past the last column use the key `anchor.seq.len()`.
    ///
    /// The anchor's own map is empty.
    member_inserts: Vec<BTreeMap<usize, Vec<usize>>>,
    /// Cumulative LCS-DP cells (`Σ |anchor|·|member|`) the star align actually charged against the
    /// shared [`CellBudget`]. [`anti_unify`] seeds its re-descent budget with this so the
    /// matched-statement re-descent CONTINUES from where the parent star-align left off (one
    /// budget across the whole per-class anti-unify), rather than restarting at a full budget.
    spent_cells: u64,
}

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
fn align_to_anchor_with_budget(
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
fn anti_unify_with_budget(
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

/// A per-span metavar before recurrence collapse.
struct RunMetavar {
    lo: usize,
    snapped_kind: &'static str,
    per_member_values: Vec<String>,
    kind: MetavarKind,
    type_hint: Option<String>,
    confidence: Confidence,
    /// Whether the differing-callee guard fired for this run (Fix 5). Threaded into the collapsed
    /// `VariationPoint.differing_callee`.
    differing_callee: bool,
    /// The syntactic type context — the recovered `: T` annotation text at this occurrence column,
    /// or `None` when the hole has no nearby annotation (Fix 3, Codex round-7). Part of the
    /// [`collapse_recurring`] key so two occurrences with the same value tuple + role but
    /// DIFFERENT annotations do NOT collapse into one metavar (which would reuse one param
    /// across two distinct typed slots). NOT carried onto the final `VariationPoint` — it only
    /// disambiguates collapse.
    type_context: Option<String>,
}

/// One span emitted by [`emit_metavar_spans`], driving exactly one candidate metavar.
///
/// - `Raw(lo, hi)` — the normal subtree span; `anti_unify` recovers its `per_member_values` from
///   the token `col_map` and classifies it (`value_param` / `closure_param` / `type_param` /
///   `gapped`).
/// - `Statement` — a STATEMENT-SNAPPED Type-3 indel: the differing region snapped to whole
///   statement boundaries and its `per_member_values` were recovered by clean statement-level
///   structural alignment (not the LCS-tangled token `col_map`). It is always a `gapped` metavar
///   and `anti_unify` consumes its values verbatim. See [`emit_block_statement_indel`] for why the
///   raw token path mangles these (closing-punctuation `( ) ;` / `}` LCS-matches across the
///   statement boundary, scrambling per-member slices and leaking single-member content into fixed
///   text). `zero_width` marks a MEMBER-ONLY inserted statement (absent from the anchor): it
///   occupies no anchor column, so it must NOT fill the coverage mask or consume the column it
///   renders next to — it renders as a hole positioned right after column `lo`.
/// - `Classified` — a PRE-CLASSIFIED inner VP from the matched-statement re-descent (Fix 2): a
///   matched statement inside an indel block was anti-unified on its own sub-range, and its
///   sub-template's VP was translated into the parent column space. `anti_unify` consumes its kind
///   / values / confidence verbatim (the parent's tangled indel-block col_map is NOT consulted,
///   same as `Statement`), but unlike `Statement` it can be any role (value/closure/type/gapped).
///   Its `[lo..=hi]` span and `zero_width` flag are the sub-VP's REAL occurrence span carried out
///   on [`Template::occurrence_spans`] (Codex round-7), NOT re-derived — so a straddling
///   multi-subtree inner hole keeps its full span and an inner member-only insert stays zero-width.
///   A consuming (`zero_width = false`) classified VP fills the coverage mask `[lo..=hi]`; a
///   `zero_width = true` one occupies no anchor column (same as `Statement { zero_width: true }`).
enum EmittedSpan {
    Raw(usize, usize),
    Statement {
        lo: usize,
        hi: usize,
        per_member_values: Vec<String>,
        zero_width: bool,
    },
    Classified {
        lo: usize,
        hi: usize,
        per_member_values: Vec<String>,
        kind: MetavarKind,
        type_hint: Option<String>,
        confidence: Confidence,
        differing_callee: bool,
        /// `true` when the re-descended sub-VP was a MEMBER-ONLY zero-width insert (a statement
        /// present in some members but absent from the parent anchor's matched statement). It
        /// occupies no anchor column, so it must NOT fill the coverage mask and renders after `lo`
        /// without consuming it — same posture as `Statement { zero_width: true }` (Fix 2, Codex
        /// round-7). A normal consuming sub-VP is `false`.
        zero_width: bool,
    },
}

impl EmittedSpan {
    fn lo(&self) -> usize {
        match *self {
            EmittedSpan::Raw(lo, _)
            | EmittedSpan::Statement { lo, .. }
            | EmittedSpan::Classified { lo, .. } => lo,
        }
    }

    fn hi(&self) -> usize {
        match *self {
            EmittedSpan::Raw(_, hi)
            | EmittedSpan::Statement { hi, .. }
            | EmittedSpan::Classified { hi, .. } => hi,
        }
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

/// `true` for a statement-like node kind — the granularity an inserted/removed statement snaps to.
/// Punctuation children of a block (`{`, `}`) are NOT statements, so they never become a snap unit.
fn is_statement_kind(kind: &str) -> bool {
    matches!(
        kind,
        "expression_statement"
            | "let_declaration"
            | "let_statement"
            | "return_statement"
            | "macro_invocation"
            | "if_expression"
            | "match_expression"
            | "for_expression"
            | "while_expression"
            | "loop_expression"
            | "item"
            | "use_declaration"
    )
}

/// The statement-like direct children of the anchor container `[lo..=hi]`, each as a
/// `(stmt_lo, stmt_hi)` column span. Non-statement children (the `{` / `}` punctuation) are
/// dropped.
fn anchor_statement_children(anchor: &RefineMember, lo: usize, hi: usize) -> Vec<(usize, usize)> {
    direct_children(anchor, lo, hi)
        .into_iter()
        .filter(|&(clo, _chi)| is_statement_kind(anchor.node_spans[clo].kind))
        .collect()
}

/// One statement extracted from a member's own AST: a structural KIND-skeleton (node kinds only,
/// dropping leaf identity) plus its real source slice.
///
/// The skeleton — NOT the struct-hash — is the match key. A struct-hash includes alpha-rename ID
/// numbering and literal buckets, so two Type-2 variant statements (`a.map(x, y, x)` vs
/// `a.map(x, y, y)`, or `foo()` vs `bar()`) get DIFFERENT struct-hashes and would (wrongly) read as
/// an insert+delete instead of one matched, substituted statement. The kind-skeleton is leaf-blind:
/// same shape ⟹ matched, so the statement-snap only gaps GENUINELY inserted/removed statements.
struct MemberStatement {
    skeleton: String,
    source: String,
    /// The statement's token range in the MEMBER's own `seq`/`node_spans`: `[token_start ..
    /// token_start + token_len)`. Used by the matched-statement re-descent (Fix 2): a matched
    /// statement is anti-unified on JUST its own token sub-range, so an inner value/callee/type/
    /// literal diff inside a matched statement still surfaces as a classified VP.
    token_start: usize,
    token_len: usize,
}

/// The structural KIND-skeleton of a statement's token subsequence: the node kinds joined, leaf
/// identity dropped (an `ID<n>` leaf or `LIT_<KIND>` leaf folds to its node kind via the parallel
/// `node_spans`, so alpha-rename and literal-value differences don't change the skeleton). This is
/// the statement match key (see [`MemberStatement`]).
fn statement_skeleton(spans: &[NodeSpan]) -> String {
    let mut s = String::new();
    for sp in spans {
        s.push_str(sp.kind);
        s.push('\u{1}');
    }
    s
}

/// Extract a member's statement-like nodes within ITS block that corresponds to the anchor
/// container column `block_col`. Recovers the member's block node via the matched token
/// (`col_map[m][block_col]`), then walks the member's `node_spans` for the statement-kind direct
/// children of that block (byte-span nesting; one level deep). Each statement carries its
/// kind-skeleton (match key) and source slice — the inputs to the statement-level structural
/// alignment. `None` when the member did not match the block column (can't locate its block).
fn member_statements(
    member: &RefineMember,
    alignment: &ClassAlignment,
    m_idx: usize,
    block_col: usize,
) -> Option<Vec<MemberStatement>> {
    let block_j = alignment.col_map[m_idx][block_col]?;
    let block_end = member.node_spans[block_j].end_byte;

    let mut stmts = Vec::new();
    let mut j = block_j + 1;
    while j < member.node_spans.len() && member.node_spans[j].start_byte < block_end {
        let sp = &member.node_spans[j];
        if is_statement_kind(sp.kind) {
            // The statement subtree is the contiguous run of tokens inside this statement's span.
            let stmt_byte_end = sp.end_byte;
            let stmt_start = j;
            let mut k = j + 1;
            while k < member.node_spans.len() && member.node_spans[k].start_byte < stmt_byte_end {
                k += 1;
            }
            let source = member
                .text
                .get(sp.start_byte..sp.end_byte)
                .map(str::to_string)
                .unwrap_or_else(|| member.seq[stmt_start..k].join(" "));
            stmts.push(MemberStatement {
                skeleton: statement_skeleton(&member.node_spans[stmt_start..k]),
                source,
                token_start: stmt_start,
                token_len: k - stmt_start,
            });
            j = k;
        } else {
            j += 1;
        }
    }
    Some(stmts)
}

/// A member's statement matched (by kind-skeleton) to one anchor statement — its real source slice
/// plus its token range in the member's own `seq`/`node_spans`. The token range feeds the
/// matched-statement re-descent (Fix 2): anti-unifying the matched statement on JUST this
/// sub-range.
struct MatchedStmt {
    source: String,
    token_start: usize,
    token_len: usize,
}

/// Outcome of statement-level structural alignment for one member against the anchor statements.
struct MemberStmtAlign {
    /// `matched[i]` = the member's statement matched to anchor statement `i` (`Some` when the
    /// member has a structurally-equal statement; `None` when the member gaps it). Carries the
    /// source slice (the gapped-statement value) AND the member's token range (the re-descent
    /// sub-range, Fix 2).
    matched: Vec<Option<MatchedStmt>>,
    /// Member statements with no structural match in the anchor — pure member-only inserts, in
    /// member order. Each is `(after_anchor_stmt_idx, source)`: the inserted statement follows
    /// anchor statement index `after_anchor_stmt_idx` (`0` when it leads all statements). Used to
    /// attach a gapped metavar at the right anchor position.
    inserts: Vec<(usize, String)>,
}

/// Align a member's statements to the anchor statements by kind-skeleton LCS (clean: a whole
/// statement matches structurally or it doesn't — no closing-punctuation tangle; leaf-blind so a
/// Type-2 substituted statement still matches). Returns, per anchor statement, the member's
/// matching source (or `None` = gapped), plus the member-only inserted statements with the anchor
/// position they follow.
///
/// KNOWN LIMITATION — leftmost-tie attribution (residual; #235 GumTree is the real fix, NOT a
/// blocker). The match is by kind-SKELETON ([`lcs_pairs`]), and `lcs_pairs` resolves ties LEFTMOST.
/// When sibling statements share a skeleton — the COMMON bare-call case, where `a();`, `b();`,
/// `c();` all skeletonize identically — the LCS cannot tell WHICH statement a member has vs gaps,
/// so it mis-attributes the position. Concretely, `a(); c();` vs `a(); b(); c();` renders
/// `a(); c(); ⟨m0?⟩` with values `["", "c();"]` and coverage 1.0: the inserted MIDDLE `b();` is
/// dropped from all values and the coverage is falsely 1.0. Statements identical under
/// normalization (`a();`/`b();`/`c();` all normalize to `ID0();`) are genuinely indistinguishable
/// to LCS; the real fix is move-aware tree diff (GumTree, #235) — DO NOT attempt to fix it here,
/// and do not relax the indel gate or revert to the raw-straddle path to paper over it.
///
/// WHY THIS IS NON-BLOCKING (display fidelity only; never a scoring over-claim):
/// 1. `lcs_ratio` (the NiCad class fidelity) is computed INDEPENDENTLY from the raw `m.seq` token
///    sequences in `cache.rs` (`class_lcs_ratio`), NOT from this template/coverage — so a dropped
///    statement still depresses the pairwise LCS.
/// 2. A gapped VP is still emitted, and `confidence_v2`/`refactorability_v2` ALWAYS downgrade on
///    `gapped > 0`. So a false coverage 1.0 can never escalate confidence to High or
///    refactorability to full — it only fails to ADD an extra downgrade it arguably should have.
///    The damage is confined to which statement renders as fixed text, the gap value, and the
///    coverage column.
///
/// The test `indel_sibling_skeleton_tie_is_balanced_but_attribution_is_approximate` pins this
/// boundary.
fn align_member_statements(
    anchor_skeletons: &[String],
    member_stmts: &[MemberStatement],
) -> MemberStmtAlign {
    let a: Vec<&str> = anchor_skeletons.iter().map(String::as_str).collect();
    let b: Vec<&str> = member_stmts.iter().map(|s| s.skeleton.as_str()).collect();
    let pairs = lcs_pairs(&a, &b);

    let mut matched: Vec<Option<MatchedStmt>> = (0..anchor_skeletons.len()).map(|_| None).collect();
    let mut matched_b: Vec<bool> = vec![false; member_stmts.len()];
    for &(ai, bj) in &pairs {
        matched[ai] = Some(MatchedStmt {
            source: member_stmts[bj].source.clone(),
            token_start: member_stmts[bj].token_start,
            token_len: member_stmts[bj].token_len,
        });
        matched_b[bj] = true;
    }
    // Member-only inserts: each unmatched member statement, attributed after the most recent
    // matched anchor statement at or before it (0 when none precedes). Walk member statements
    // in order, tracking the running "after" position from the matched pairs.
    let mut inserts = Vec::new();
    let mut after = 0usize;
    let mut pair_iter = pairs.iter().peekable();
    for (bj, stmt) in member_stmts.iter().enumerate() {
        while let Some(&&(ai, pbj)) = pair_iter.peek() {
            if pbj <= bj {
                after = ai + 1;
                pair_iter.next();
                if pbj == bj {
                    break;
                }
            } else {
                break;
            }
        }
        if !matched_b[bj] {
            inserts.push((after, stmt.source.clone()));
        }
    }
    MemberStmtAlign { matched, inserts }
}

/// LCS over two `&str` slices → the matched `(ai, bj)` index pairs, ascending. A tiny DP (statement
/// counts are small — ≤ a function body's statement count). Deterministic.
///
/// Tie-break is LEFTMOST (the `>=` in the backtrack). When several elements are equal (skeleton-LCS
/// over sibling bare calls) the leftmost match wins, which mis-attributes WHICH statement a member
/// gaps — see the KNOWN LIMITATION note on [`align_member_statements`] (display-fidelity residual,
/// GumTree #235 is the real fix, non-blocking for scoring).
fn lcs_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] =
                if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Statement-snap a block-level Type-3 indel (rule 5). Returns `true` when it handled the node
/// (emitted clean statement metavars / left matched statements fixed), `false` when there is no
/// statement-level indel here (every anchor statement matched by every aligned member, no
/// member-only insert) — the caller then falls through to the generic decomposable path (rule 6)
/// for plain substitutions.
///
/// The headline correctness fix (#215 Plan 4b): an inserted statement whose neighbour ends in `();`
/// tangles the flat token LCS (`( ) ;` matches across the statement boundary), so the raw col_map
/// produces mangled, ordinal-scrambled per-member values and leaks single-member content into fixed
/// template text. Here we re-derive at STATEMENT granularity via kind-skeleton LCS, so:
/// - a matched statement stays FIXED (its tangled within-statement col_map is NOT trusted; a
///   structurally-equal statement has no genuine sub-variation, and a co-occurring literal/callee
///   diff in a matched neighbour is a documented simplification — Fix-2/differing-callee still fire
///   in non-indel blocks, which take rule 6),
/// - a statement some member gaps becomes ONE gapped metavar over its WHOLE span with balanced,
///   ordinal-correct values (`""` for the absent member, the whole statement for the present one),
/// - a member-only inserted statement becomes ONE zero-width gapped metavar attributed at the
///   anchor position it follows.
#[allow(clippy::too_many_arguments)] // threads the shared cell budget + sample flag for re-descent.
fn emit_block_statement_indel(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    anchor: &RefineMember,
    _is_fixed: &[bool],
    lo: usize,
    hi: usize,
    budget: &mut CellBudget,
    redescent_sampled: &mut bool,
    out: &mut Vec<EmittedSpan>,
) -> bool {
    let stmt_children = anchor_statement_children(anchor, lo, hi);
    if stmt_children.is_empty() {
        return false;
    }
    let anchor_skeletons: Vec<String> = stmt_children
        .iter()
        .map(|&(slo, shi)| statement_skeleton(&anchor.node_spans[slo..=shi]))
        .collect();

    // Gather each ALIGNED non-anchor member's own statements. A member whose block we can't locate
    // (didn't match the block column) is excluded — it can't witness a clean statement indel.
    // Skipped (cost-capped) members are likewise excluded.
    let mut member_stmt_lists: Vec<Option<Vec<MemberStatement>>> =
        Vec::with_capacity(members.len());
    for (m_idx, _member) in members.iter().enumerate() {
        if !alignment.aligned[m_idx] || m_idx == alignment.anchor_idx {
            member_stmt_lists.push(None);
        } else {
            member_stmt_lists.push(member_statements(&members[m_idx], alignment, m_idx, lo));
        }
    }

    // INDEL GATE: only snap when an aligned member's statement COUNT differs from the anchor's —
    // the honest signal of an inserted/removed statement. When every aligned member has the
    // same count, each statement is a 1:1 positional SUBSTITUTION (e.g. `g(process(x))` vs
    // `g(y.trim())` — one statement, different internal structure), NOT an indel: defer to rule
    // 6 so the substitution is classified normally. This is what keeps the snap from
    // mis-reading a substituted statement as a gapped insert+delete.
    let anchor_count = stmt_children.len();
    let count_differs = member_stmt_lists
        .iter()
        .enumerate()
        .any(|(m, l)| alignment.aligned[m] && l.as_ref().is_some_and(|s| s.len() != anchor_count));
    if !count_differs {
        return false;
    }

    // Per aligned member, align its statements to the anchor statements (kind-skeleton LCS).
    let mut per_member: Vec<Option<MemberStmtAlign>> = Vec::with_capacity(members.len());
    for (m_idx, list) in member_stmt_lists.into_iter().enumerate() {
        if !alignment.aligned[m_idx] {
            per_member.push(None);
            continue;
        }
        if m_idx == alignment.anchor_idx {
            // The anchor matches every statement by identity (its own columns); no inserts.
            per_member.push(Some(MemberStmtAlign {
                matched: stmt_children
                    .iter()
                    .map(|&(slo, shi)| {
                        anchor
                            .text
                            .get(anchor.node_spans[slo].start_byte..anchor.node_spans[shi].end_byte)
                            .map(|source| MatchedStmt {
                                source: source.to_string(),
                                token_start: slo,
                                token_len: shi - slo + 1,
                            })
                    })
                    .collect(),
                inserts: Vec::new(),
            }));
            continue;
        }
        per_member.push(list.map(|stmts| align_member_statements(&anchor_skeletons, &stmts)));
    }

    // Is there a genuine statement-level indel? (some anchor statement gapped by an aligned member,
    // OR some member-only inserted statement). The count gate above already implies one, but a
    // skeleton mismatch could in principle align everything 1:1 — re-check to be safe.
    let any_gap = per_member
        .iter()
        .flatten()
        .any(|a| a.matched.iter().any(Option::is_none) || !a.inserts.is_empty());
    if !any_gap {
        return false;
    }

    // One EmittedSpan per anchor statement that some member gaps; matched statements stay FIXED
    // (a gapped statement's tangled within-statement col_map is not trusted), but a MATCHED
    // statement is RE-DESCENDED (Fix 2): it is anti-unified on JUST its own token sub-range so an
    // inner value/callee/type/literal diff inside it still surfaces as a classified VP.
    for (si, &(slo, shi)) in stmt_children.iter().enumerate() {
        let gapped_by_some = per_member.iter().enumerate().any(|(m, a)| {
            alignment.aligned[m] && a.as_ref().is_some_and(|a| a.matched[si].is_none())
        });
        if gapped_by_some {
            // Whole-statement gapped metavar: ordinal-correct, balanced (whole statement or "").
            let per_member_values: Vec<String> = (0..members.len())
                .map(|m| match per_member[m].as_ref() {
                    Some(a) => a.matched[si].as_ref().map(|s| s.source.clone()).unwrap_or_default(),
                    None => String::new(),
                })
                .collect();
            out.push(EmittedSpan::Statement {
                lo: slo,
                hi: shi,
                per_member_values,
                zero_width: false,
            });
        } else {
            // MATCHED by every aligned member (Fix 2): re-descend into the statement's own token
            // sub-range so an inner diff (`p(1)` vs `p(2)`, a differing callee, a type swap) is
            // anti-unified and emitted as a classified VP — instead of being silently dropped by
            // leaving the whole matched statement fixed. The re-descent draws from the SHARED
            // per-class `budget`: once it is exhausted it leaves the statement whole-fixed (no
            // extra exact DP) and latches `redescent_sampled` so the class is honestly
            // flagged.
            emit_matched_statement_redescent(
                members,
                alignment.anchor_idx,
                &per_member,
                si,
                slo,
                shi,
                budget,
                redescent_sampled,
                out,
            );
        }
    }

    // Member-only inserted statements (anchor lacks them entirely) → zero-width gapped metavars.
    emit_member_only_inserts(&stmt_children, &per_member, members.len(), out);
    true
}

/// Re-descend a MATCHED anchor statement `si` (anchor columns `[slo..=shi]`) that every aligned
/// member realises with a structurally-equal statement — anti-unify it on JUST its own token
/// sub-range so an inner value/callee/type/literal diff still surfaces as a classified VP (Fix 2,
/// #215 Plan 4b Codex round-5).
///
/// WHY a fresh sub-alignment, not the global col_map: the statement-snap engages precisely because
/// the flat token LCS is TANGLED across statement boundaries (closing `();`/`}` matches the wrong
/// statement), so the global `alignment.col_map` cannot be trusted inside an indel block. WITHIN a
/// matched statement the structure is clean, so we re-extract each member's statement as a
/// sub-member (a slice of its `seq`/`node_spans` keeping the same ABSOLUTE byte offsets, so
/// recovered source and rendered text stay correct) and run the SAME `align_to_anchor` +
/// `anti_unify` pipeline on the sub-members. Each sub-template VP is then translated back into the
/// parent anchor's column space (sub-column `c` becomes parent column `slo + c`) and full member
/// ordinality (a member that gaps this statement, or is cost-skipped, contributes `""`), then
/// emitted as a pre-classified `EmittedSpan::Classified`.
///
/// SUB-ANCHOR = THE PARENT ANCHOR'S OWN STATEMENT (Fix 1, Codex round-7). The translation
/// `parent_lo = slo + sub_col` is exact ONLY when the sub-template's columns ARE
/// `anchor.node_spans[slo..=shi]` — i.e. the sub-alignment is anchored on the PARENT anchor's
/// (medoid's) statement slice, not on the canonical-first matched member. When the medoid is not
/// canonical member 0, anchoring on the first sub-member would spine the sub-alignment on a
/// different member's statement and shift `slo + sub_col` onto the wrong medoid columns. We pass
/// `parent_anchor_idx` (`alignment.anchor_idx`) and use the sub-member it built as the sub-anchor.
#[allow(clippy::too_many_arguments)] // threads the shared cell budget + sample flag.
fn emit_matched_statement_redescent(
    members: &[RefineMember],
    parent_anchor_idx: usize,
    per_member: &[Option<MemberStmtAlign>],
    si: usize,
    slo: usize,
    shi: usize,
    budget: &mut CellBudget,
    redescent_sampled: &mut bool,
    out: &mut Vec<EmittedSpan>,
) {
    // Aggregate cell budget (shared with the parent star-align): once the per-class budget is
    // exhausted, STOP re-descending matched statements — leave this one whole-fixed (the documented
    // pre-Fix-2 behavior: an inner diff inside it is not surfaced as a VP) instead of running
    // another exact sub-DP. Latch `redescent_sampled` so the class is honestly flagged sampled.
    // This is what bounds the WHOLE per-class anti-unify (parent align + all re-descents) by
    // ONE budget.
    if budget.exhausted {
        *redescent_sampled = true;
        return;
    }
    // Build a sub-member per member that matched this statement: a slice of its own seq/node_spans
    // for the statement's token range (same ABSOLUTE byte offsets). `sub_to_full[k]` maps the k-th
    // sub-member back to its full member ordinal, so the sub-template's per_member_values can be
    // re-expanded. Sub-members preserve the parent's canonical order, so the sub-anchor is stable.
    //
    // `sub_anchor_pos` records WHERE in `sub_members` the PARENT anchor's (medoid's) own statement
    // landed — it is the sub-anchor (Fix 1, #215 Plan 4b Codex round-7). The translation below maps
    // a sub-column `c` to parent column `slo + c`, which is exact ONLY when the sub-anchor's
    // `node_spans` ARE `anchor.node_spans[slo..=shi]` — i.e. the sub-anchor is the parent anchor's
    // own statement slice. Using the canonical-first sub-member instead would, when the medoid is
    // not canonical member 0, anchor the sub-alignment on a DIFFERENT member's statement spine, so
    // `slo + sub_col` would index the medoid anchor's columns at the wrong positions → wrong
    // rendered/classified source. The parent anchor always matches its own statement (it is the
    // anchor; `per_member[anchor_idx].matched[si]` is always `Some`), so it is always present here.
    let mut sub_members: Vec<RefineMember> = Vec::new();
    let mut sub_to_full: Vec<usize> = Vec::new();
    let mut sub_anchor_pos: Option<usize> = None;
    for (m_idx, member) in members.iter().enumerate() {
        let Some(matched) = per_member[m_idx].as_ref().and_then(|a| a.matched[si].as_ref()) else {
            continue;
        };
        let start = matched.token_start;
        let end = start + matched.token_len;
        if end > member.seq.len() || end > member.node_spans.len() || start >= end {
            // Defensive: a malformed range can't be re-descended — bail on the whole statement
            // rather than emit a partial/incorrect sub-template (the statement stays fixed).
            return;
        }
        if m_idx == parent_anchor_idx {
            sub_anchor_pos = Some(sub_members.len());
        }
        sub_members.push(RefineMember {
            symbol_id: member.symbol_id,
            lang: member.lang,
            struct_hash: member.struct_hash.clone(),
            seq: member.seq[start..end].to_vec(),
            node_spans: member.node_spans[start..end].to_vec(),
            text: member.text.clone(),
        });
        sub_to_full.push(m_idx);
    }
    // Need ≥2 sub-members to have any variation; with <2 there is nothing to anti-unify.
    if sub_members.len() < 2 {
        return;
    }
    // The sub-anchor MUST be the parent anchor's own statement slice (Fix 1): only then are the
    // sub-template's columns genuinely `anchor.node_spans[slo + c]`, making `parent_lo = slo +
    // sub_col` exact. Defensive fall back to the canonical-first sub-member if the parent anchor's
    // statement is somehow absent (it never is — the anchor matches every statement by identity).
    let sub_anchor_idx = sub_anchor_pos.unwrap_or_else(|| resolve_anchor_idx(&sub_members, None));

    // Anti-unify the sub-members with the SAME pipeline (the reuse the prompt sanctions), threading
    // the SHARED per-class `budget` so the sub-align's cells and any deeper (nested-statement)
    // re-descent draw from the one budget — NOT a fresh one.
    let sub_alignment = align_to_anchor_with_budget(&sub_members, sub_anchor_idx, budget);
    // The sub-align may itself have hit the budget (a member skipped past the cutover) → fold its
    // sampling into the class flag; the budget-aware `anti_unify_with_budget` uses the same shared
    // budget for any deeper re-descent (no re-seed — the sub-alignment's cells are already
    // charged).
    *redescent_sampled |= sub_alignment.sampled;
    let sub_template = anti_unify_with_budget(&sub_members, &sub_alignment, budget);
    *redescent_sampled |= sub_template.sampled;

    // Translate each sub-VP into the parent: shift each occurrence column by `slo` (the
    // sub-anchor's node_spans are exactly `anchor.node_spans[slo..=shi]`, so sub-column c is
    // parent column slo+c) and re-expand per_member_values to full member ordinality. Each VP
    // is emitted as a pre-classified span carrying its kind/values verbatim (the col_map for
    // the parent's tangled indel block is NOT consulted — the sub-template already classified
    // cleanly).
    for vp in &sub_template.variation_points {
        // Re-expand sub-ordinal values to full member ordinality: a member absent from the
        // sub-class (gapped this statement, or cost-skipped) contributes "".
        let mut full_values = vec![String::new(); members.len()];
        for (sub_idx, &full_idx) in sub_to_full.iter().enumerate() {
            if let Some(v) = vp.per_member_values.get(sub_idx) {
                full_values[full_idx] = v.clone();
            }
        }
        for &sub_col in &vp.occurrences {
            let parent_lo = slo + sub_col;
            if parent_lo > shi {
                continue;
            }
            // Carry the sub-VP's ACTUAL snapped span + zero-width flag (Fix 2, Codex round-7) from
            // `occurrence_spans` rather than re-deriving `hi` from the parent subtree at
            // `parent_lo`. Re-derivation TRUNCATED a straddling multi-subtree hole (a span wider
            // than the one subtree at its start column — e.g. a differing `a + b` vs `c * d` that
            // snapped wider than `parent_lo`'s subtree) and turned a zero-width member-only insert
            // (a statement absent from the anchor's matched statement) into a CONSUMING hole. The
            // sub-template's columns ARE `anchor.node_spans[slo..]` (sub-anchor = parent anchor's
            // statement, Fix 1), so `slo + sub_hi` is the correct parent hi; bound by `shi`.
            // Defensive fall back to a single-leaf span if the occurrence is somehow missing.
            let occ = sub_template.occurrence_spans.get(&sub_col).copied();
            let parent_hi = occ.map(|o| slo + o.hi).unwrap_or(parent_lo).min(shi);
            let zero_width = occ.is_some_and(|o| o.zero_width);
            out.push(EmittedSpan::Classified {
                lo: parent_lo,
                hi: parent_hi,
                per_member_values: full_values.clone(),
                kind: vp.kind,
                type_hint: vp.type_hint.clone(),
                confidence: vp.confidence,
                differing_callee: vp.differing_callee,
                zero_width,
            });
        }
    }
}

/// Emit zero-width gapped metavars for member-only inserted statements — statements present in some
/// members' source but ABSENT from the anchor (anchor is the shorter member). Each distinct
/// follow-position becomes one metavar attributed at the END column of the anchor statement it
/// follows (or the first statement's start when it leads), with per-member values = the inserted
/// source (or `""`).
fn emit_member_only_inserts(
    stmt_children: &[(usize, usize)],
    per_member: &[Option<MemberStmtAlign>],
    member_count: usize,
    out: &mut Vec<EmittedSpan>,
) {
    let mut positions: Vec<usize> = per_member
        .iter()
        .flatten()
        .flat_map(|a| a.inserts.iter().map(|&(after, _)| after))
        .collect();
    positions.sort_unstable();
    positions.dedup();

    for after in positions {
        let per_member_values: Vec<String> = (0..member_count)
            .map(|m| match per_member[m].as_ref() {
                Some(a) => a
                    .inserts
                    .iter()
                    .filter(|&&(p, _)| p == after)
                    .map(|(_, s)| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => String::new(),
            })
            .collect();
        // Attach at the END column of the statement this insert follows (renders right after it),
        // or the first statement's lo when it leads. `after` is a statement INDEX;
        // `after-1` precedes.
        let attach = if after == 0 {
            stmt_children.first().map(|&(slo, _)| slo).unwrap_or(0)
        } else {
            stmt_children
                .get(after - 1)
                .map(|&(_, shi)| shi)
                .or_else(|| stmt_children.last().map(|&(_, shi)| shi))
                .unwrap_or(0)
        };
        out.push(EmittedSpan::Statement {
            lo: attach,
            hi: attach,
            per_member_values,
            zero_width: true,
        });
    }
}

/// Coalesce the variation columns of `[lo..=hi]` into maximal contiguous runs `(rlo, rhi)`. A
/// column is a variation column iff it is non-fixed for some member OR any member keys an insert at
/// it. Runs are returned column-ascending and non-overlapping.
fn variation_runs(
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
fn subtree_is_indel(
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
fn any_member_inserts_within(alignment: &ClassAlignment, lo: usize, hi: usize) -> bool {
    if lo > hi {
        return false;
    }
    alignment.member_inserts.iter().any(|ins| ins.range(lo..=hi).next().is_some())
}

/// The direct children of the anchor node whose subtree is `[lo..=hi]`: the maximal contiguous
/// sub-spans of `[lo+1..=hi]`, each one child subtree (pre-order, byte-span nesting).
fn direct_children(anchor: &RefineMember, lo: usize, hi: usize) -> Vec<(usize, usize)> {
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
fn subtree_token_count(anchor: &RefineMember, root: usize) -> usize {
    let root_end = anchor.node_spans[root].end_byte;
    let mut count = 1;
    let mut k = root + 1;
    while k < anchor.node_spans.len() && anchor.node_spans[k].start_byte < root_end {
        count += 1;
        k += 1;
    }
    count
}

/// The string-BODY leaf kinds whose value the baseline normalizer erases (buckets to
/// `LIT_STRING_CONTENT` / `LIT_STRING_FRAGMENT`): Rust/Python `string_content` and TS/JS
/// `string_fragment` (#232 #2a). Both are the inner text leaf that needs widening to its enclosing
/// quote-bearing node so the hole covers the WHOLE `"hello"`.
fn is_string_body_leaf_kind(kind: &str) -> bool {
    matches!(kind, "string_content" | "string_fragment")
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
fn is_string_node_kind(kind: &str) -> bool {
    matches!(kind, "string_literal" | "string" | "template_string")
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
/// widen gate refuses to cross — see [`is_string_node_kind`].
fn is_string_delimiter_or_body_leaf_kind(kind: &str) -> bool {
    is_string_body_leaf_kind(kind) || matches!(kind, "\"" | "`" | "'" | "escape_sequence")
}

/// `true` when the anchor subtree rooted at `str_col` contains a `template_substitution` — i.e. it
/// is an INTERPOLATED template literal (`` `hi${x}lo` ``). Widening a fragment/quote run to such a
/// node would swallow the `${…}` interpolation into the hole, so [`widen_string_content_run`] does
/// not widen across it (#254 caution). Walks the contiguous pre-order subtree by byte containment,
/// mirroring [`subtree_token_count`].
fn string_node_has_interpolation(anchor: &RefineMember, str_col: usize) -> bool {
    let root_end = anchor.node_spans[str_col].end_byte;
    let mut k = str_col + 1;
    while k < anchor.node_spans.len() && anchor.node_spans[k].start_byte < root_end {
        if anchor.node_spans[k].kind == "template_substitution" {
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
fn widen_string_content_run(anchor: &RefineMember, lo: usize, hi: usize) -> (usize, usize) {
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
fn widen_generic_type_head_run(anchor: &RefineMember, lo: usize, hi: usize) -> (usize, usize) {
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
fn annotation_type_context(anchor: &RefineMember, lo: usize) -> Option<String> {
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

/// The role a reopened matched column takes — the output of [`matched_column_reopen`]. Each variant
/// names a position the baseline normalizer ERASES the source value of, so a genuine difference
/// hides behind an LCS-matched (and therefore "fixed") column. `classify_run` independently derives
/// the same role from the anchor node kind / callee position, so this is the reopen-decision side
/// of one source of truth, not a second classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReopenRole {
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
fn matched_column_reopen(
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
    matched_source_values_differ(members, alignment, col).then_some(role)
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
fn aligned_values<'a>(
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
fn aligned_values_all_equal(per_member_values: &[String], alignment: &ClassAlignment) -> bool {
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
fn recover_values(
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

/// Classification outcome for one run: its extraction role, an optional type hint, the confidence
/// band, and whether the differing-callee guard fired (the Plan-3 SCIP seam).
struct RunClass {
    kind: MetavarKind,
    type_hint: Option<String>,
    confidence: Confidence,
    /// `true` ONLY when the differing-callee guard fired (a genuine differing callee/method-name).
    /// Carried explicitly so downstream scoring never re-infers it from `(kind, confidence)` — Fix
    /// 5 (a `ClosureParam` at `Medium` may instead be a generic closure-ish subtree).
    differing_callee: bool,
}

/// Classify a run's extraction role (§1.8), precedence `gapped > closure_param > type_param >
/// value_param`.
fn classify_run(
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
    if run_callees_differ(per_member_values) {
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
fn run_in_callee_position(anchor: &RefineMember, lo: usize, hi: usize) -> bool {
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
fn is_type_position(kind: &str) -> bool {
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
fn collapse_recurring(candidates: Vec<RunMetavar>) -> Vec<VariationPoint> {
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

/// Render the human-readable template from the anchor's real source (§1.9): a maximal fixed run is
/// the verbatim source slice (collapsing inter-token whitespace via `prev.end .. next.start`); a
/// variation run is `⟨m{id}⟩`; a gapped run is `⟨m{id}?⟩`. `lo_to_hi` maps each occurrence's lo
/// column to its snapped span end. `zero_width_cols` are columns where a MEMBER-ONLY inserted
/// statement attaches: its hole renders AFTER the attach column's fixed token without consuming it
/// (the anchor has no column for it — it is appended source, not a substitution).
fn render_template(
    anchor: &RefineMember,
    variation_points: &[VariationPoint],
    lo_to_hi: &BTreeMap<usize, usize>,
    zero_width_cols: &std::collections::BTreeSet<usize>,
) -> String {
    let spine_len = anchor.node_spans.len();
    struct Occ {
        lo: usize,
        hi: usize,
        label: String,
        gapped: bool,
        /// Member-only insert: render after the attach column's token, don't consume it.
        zero_width: bool,
    }
    let mut occs: Vec<Occ> = Vec::new();
    for vp in variation_points {
        let gapped = vp.kind == MetavarKind::Gapped;
        for &lo in &vp.occurrences {
            let zero_width = zero_width_cols.contains(&lo);
            let hi = if zero_width { lo } else { lo_to_hi.get(&lo).copied().unwrap_or(lo) };
            occs.push(Occ { lo, hi, label: vp.metavar_id.clone(), gapped, zero_width });
        }
    }
    occs.sort_by_key(|o| o.lo);

    let label_of = |o: &Occ| {
        if o.gapped { format!("⟨{}?⟩", o.label) } else { format!("⟨{}⟩", o.label) }
    };

    // Render every zero-width member-only insert whose attach column is in `[lo..=hi]` (after the
    // fixed token, or after a consuming hole that covers those columns), in deterministic
    // sorted-`lo`-then-`metavar_id` order (`occs` is already `lo`-sorted; ties keep emission
    // order). Shared by BOTH the consuming-hole branch and the fixed-column branch so a
    // zero-width VP is NEVER dropped from the template — Fix 4 (#215 Plan 4b Codex round-4).
    let render_zero_width_in = |out: &mut String, lo: usize, hi: usize| {
        for occ in occs.iter().filter(|o| o.zero_width && lo <= o.lo && o.lo <= hi) {
            out.push(' ');
            out.push_str(&label_of(occ));
        }
    };

    let mut out = String::new();
    let mut col = 0usize;
    let mut byte_cursor: Option<usize> = None; // end_byte of the last emitted fixed token
    while col < spine_len {
        // A consuming hole (a substituted/gapped span) starts at this column → emit it and skip the
        // span.
        if let Some(occ) = occs.iter().find(|o| o.lo == col && !o.zero_width) {
            // Emit any pending whitespace between the previous fixed token and this hole.
            let hole_start = anchor.node_spans[occ.lo].start_byte;
            if let Some(prev_end) = byte_cursor
                && let Some(ws) = anchor.text.get(prev_end..hole_start)
            {
                out.push_str(ws);
            }
            out.push_str(&label_of(occ));
            byte_cursor = Some(anchor.node_spans[occ.hi].end_byte);
            // Fix 4: a zero-width member-only insert may share this `lo` column with the consuming
            // hole — or attach anywhere within the consumed span `[occ.lo..=occ.hi]` (one member
            // deletes the first anchor statement while another inserts a leading statement before
            // it). The old code `continue`d straight to `occ.hi + 1`, skipping those columns, so
            // the zero-width VP — still present in `variation_points` JSON — got NO
            // placeholder in the template (a metavar with no rendered hole). Render
            // every zero-width insert across the consumed range here, so every VP in
            // the JSON has a placeholder in the template.
            render_zero_width_in(&mut out, occ.lo, occ.hi);
            col = occ.hi + 1;
            continue;
        }
        // Fixed column: emit only leaf tokens' source verbatim (internal-node tokens are spans of
        // their leaves — emitting them would duplicate). Use the leaf's real source slice + the
        // whitespace before it.
        let span = &anchor.node_spans[col];
        if span.is_leaf {
            if let Some(prev_end) = byte_cursor
                && let Some(ws) = anchor.text.get(prev_end..span.start_byte)
            {
                out.push_str(ws);
            }
            if let Some(src) = anchor.text.get(span.start_byte..span.end_byte) {
                out.push_str(src);
            }
            byte_cursor = Some(span.end_byte);
        }
        // Member-only inserts attached at this column render right after its token (a trailing
        // appended statement), without consuming any anchor column.
        render_zero_width_in(&mut out, col, col);
        col += 1;
    }
    out
}

/// Compute coverage directly from the fixed/variation column mask. `fixed / total`, `1.0` for an
/// empty spine. This is the authoritative coverage; `anti_unify` calls it.
fn coverage_from_mask(is_fixed: &[bool]) -> f64 {
    if is_fixed.is_empty() {
        return 1.0;
    }
    let fixed = is_fixed.iter().filter(|f| **f).count();
    fixed as f64 / is_fixed.len() as f64
}

#[cfg(test)]
mod tests;
