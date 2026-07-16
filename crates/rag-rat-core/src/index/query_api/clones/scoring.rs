//! ROI scoring, ranking gates, and the shared canonical-ordering / completeness helpers for the
//! clone query API.
//!
//! This module owns the pieces that turn a built [`CandidateCloneClass`] into a *ranked* result:
//! the coverage ROI gate ([`coverage_roi_gate`]), the un-refined member-count dampen
//! ([`dampen_unrefined_member_count`]), and the refinement application ([`apply_refinement`]) that
//! swaps the cohesion multiplier for `refactorability`. It also holds the small shared helpers that
//! several stages depend on: the deterministic class key ([`class_key_for`]), the REINDEX-STABLE
//! canonical member-order key ([`canonical_member_order_key`]), the cohesion tie-breaker
//! ([`min_pairwise_cohesion`]), and the [`CloneCompleteness`] provenance builder
//! ([`build_completeness`]).

use std::collections::BTreeMap;

use super::MEMBER_VALUE_CAP;
use super::substrate::{SymbolBag, overlap};
use super::types::{CandidateCloneClass, CloneCompleteness};
use crate::index::clones::NORM_VERSION;

/// Deterministic, order-independent class key: sort `member_refs`, join with `\n`,
/// `hex_sha256`, take the first 16 hex chars.
pub(crate) fn class_key_for(member_refs: &[String]) -> String {
    let mut sorted = member_refs.to_vec();
    sorted.sort_unstable();
    let joined = sorted.join("\n");
    rag_rat_base::hash::hex_sha256(joined.as_bytes())[..16].to_string()
}

/// The REINDEX-STABLE canonical member ordering key (#215 Plan 4b Fix 2, Codex round-4).
///
/// `(struct_hash, path, start_byte)` is the single source of truth for the ordinal basis the
/// anti-unify `per_member_values[]` align to. BOTH `IndexDatabase::load_refine_members` (which
/// orders the members the values are collected over) and `build_class`'s `canonical_member_refs`
/// (which labels each value with a member identity) sort by THIS key, so a value at ordinal `i`
/// always maps to the member ref at ordinal `i`.
///
/// Why NOT `symbol_id`: it is a `symbols.id` rowid REASSIGNED on every reindex. The 4b cache is
/// content-addressed (`refinement_key` over struct_hash + source-byte discriminators), so a
/// file-unchanged reindex serves the SAME cached `per_member_values` — frozen at the OLD member
/// order — while `canonical_member_refs` is recomputed live. If the live order keyed off
/// `symbol_id` it could differ from the frozen value order whenever two members share a struct_hash
/// (the common case in a clone class), mislabelling values. `(path, start_byte)` uniquely
/// identifies a member (no two symbols start at the same byte in one file) and is stable across a
/// file-unchanged reindex, so the two orders always agree. Keep both call sites threading through
/// this helper.
pub(crate) fn canonical_member_order_key<'a>(
    struct_hash: &'a str,
    path: &'a str,
    start_byte: i64,
) -> (&'a str, &'a str, i64) {
    (struct_hash, path, start_byte)
}

/// Minimum pairwise overlap/max_len similarity across all member pairs of `class`. This is the same
/// cohesion floor `build_class` computes for `similarity_min`, recomputed cheaply here as the
/// tie-breaker for `clones_for_symbol`'s largest-group selection (when the clique-cover split
/// returns several equal-size groups containing the subject, the most internally-cohesive wins).
/// A 1-member (or empty) class has no pairs → cohesion 1.0 (vacuously fully coherent).
pub(crate) fn min_pairwise_cohesion(class: &[i64], by_id: &BTreeMap<i64, &SymbolBag>) -> f64 {
    let mut min = f64::MAX;
    for i in 0..class.len() {
        for j in (i + 1)..class.len() {
            if let (Some(ba), Some(bb)) = (by_id.get(&class[i]), by_id.get(&class[j])) {
                let max_len = ba.token_len.max(bb.token_len);
                let sim = if max_len == 0 { 1.0 } else { overlap(ba, bb) as f64 / max_len as f64 };
                if sim < min {
                    min = sim;
                }
            }
        }
    }
    if min == f64::MAX { 1.0 } else { min }
}

/// Anti-unify coverage below which a refined class is strongly penalized in the ROI sort (#256). A
/// class under this is degenerate (almost none of the shared spine is fixed, a `⟨m0⟩`-style
/// template), so it is not a refactorable clone and must not float to the top on member count.
const COVERAGE_STRONG_GATE: f64 = 0.3;
/// Anti-unify coverage below which a refined class is mildly penalized (the band between
/// [`COVERAGE_STRONG_GATE`] and this). Mirrors `refactorability_v2`'s `< 0.5` band.
const COVERAGE_MILD_GATE: f64 = 0.5;
/// Strong refined-ROI multiplier for a near-zero-coverage (degenerate) class: an order of
/// magnitude, enough to sink a coverage-0.00 13-member helper "class" below genuine higher-coverage
/// clones without zeroing the ROI (a strictly positive factor keeps the class visible, just
/// deprioritized).
pub(crate) const COVERAGE_STRONG_PENALTY: f64 = 0.10;
/// Mild refined-ROI multiplier for the `[0.3, 0.5)` coverage band (matches `refactorability_v2`).
pub(crate) const COVERAGE_MILD_PENALTY: f64 = 0.70;

/// Mutually-exclusive coverage band giving the refined-ROI multiplier (#256): the strong penalty
/// below [`COVERAGE_STRONG_GATE`], the mild penalty in the `[0.3, 0.5)` band, and `1.0` (no
/// penalty) at/above [`COVERAGE_MILD_GATE`]. Applied ONLY to the refined ROI in
/// [`apply_refinement`]; the un-refined sort has no coverage to gate on.
///
/// This gate STACKS ON `refactorability_v2`'s own `< 0.5 → ×0.70` coverage factor — it does NOT
/// replace it. In [`apply_refinement`] the refined ROI is `… × refactorability_v2 × coverage_gate`,
/// and `refactorability_v2` is ALREADY coverage-penalized, so the two multipliers compound. The NET
/// coverage multiplier is `0.70 × 0.70 = 0.49` in the `[0.3, 0.5)` band and `0.70 × 0.10 = 0.07`
/// below `0.3`. The compounding is INTENTIONAL: `refactorability_v2`'s `×0.70` alone was too weak
/// to sink a degenerate coverage-0.00 class beneath genuine higher-coverage clones, because raw
/// `member_count` dominates — the extra gate is what makes the penalty bite (#256).
pub(crate) fn coverage_roi_gate(anti_unify_coverage: f64) -> f64 {
    if anti_unify_coverage < COVERAGE_STRONG_GATE {
        COVERAGE_STRONG_PENALTY
    } else if anti_unify_coverage < COVERAGE_MILD_GATE {
        COVERAGE_MILD_PENALTY
    } else {
        1.0
    }
}
/// Re-score a class that ended the refine pass STILL un-refined so its raw `member_count` factor
/// can no longer let it masquerade as high-ROI against the (coverage-gated) refined classes it is
/// ranked against (#259, Adversary C).
///
/// THE GAP: `find_clones` refines at most `UNLIMITED_REFINE_BUDGET` (50) classes by provisional ROI
/// and then re-sorts the WHOLE list on the SAME ROI scale. A class that ranks past the budget — OR
/// that is within budget but FAILS refinement ([`IndexDatabase::refine_class_in_place`] is a no-op
/// when refine inputs are unavailable: overlay scope, drifted source, parse failure, a vanished
/// hydration row) — keeps its un-refined Plan-2 ROI:
/// `cross_module_spread × member_count × body_token_len_medoid × load_bearing_factor ×
/// cohesion_min_pairwise`. That product is LINEAR in `member_count` and carries NO
/// [`coverage_roi_gate`] (coverage only exists post-refine), while a refined degenerate class has
/// already been knocked down `0.07–0.49×` by the gate. So a large refine-FAILED component can
/// out-rank a gated refined class purely on member count — the exact tail #256 left open.
///
/// THE FIX: dampen ONLY the `member_count` factor of an un-refined class from LINEAR to the
/// `1 + ln(1 + member_count)` sub-linear shape the codebase already uses for `load_bearing_factor`.
/// This surfaces the refine-failure as a ranking penalty that GROWS with size — exactly where the
/// masquerade is dangerous — while leaving small un-refined classes essentially untouched
/// (member_count 2 → ~2.10, a <5% nudge), so the un-refined tail of a healthy result keeps its
/// relative order. A refined class is NEVER touched (its ROI already went through the coverage gate
/// in [`apply_refinement`]); the per-class fields are unchanged, only `class.roi` (the sort key) is
/// rewritten. Because it only re-weights a factor that is already positive, a dampened class stays
/// strictly positive and visible — it is deprioritized, not dropped.
///
/// This is a RANKING-only change (#259 is a gating issue, not a perf issue): it rewrites the sort
/// key, never membership or which pairs are clones, so both clone-output parities stay green by
/// construction.
///
/// [`IndexDatabase::refine_class_in_place`]: crate::index::IndexDatabase
pub(crate) fn dampen_unrefined_member_count(class: &mut CandidateCloneClass) {
    // Defensive: a refined class already carries the refactorability × coverage_gate ROI; never
    // re-dampen it (the caller only passes un-refined classes, but keep the helper self-guarding).
    if class.refined {
        return;
    }
    let mc = class.member_count as f64;
    if mc <= 0.0 {
        return;
    }
    // Replace the linear `member_count` factor with `1 + ln(1 + member_count)`. Refold by dividing
    // out the linear factor and multiplying the dampened one so every OTHER factor
    // (cross_module_spread, body_token_len_medoid, load_bearing_factor, cohesion_min_pairwise) is
    // preserved exactly — this is a pure re-weight of the size term, not a recompute from scratch
    // (which would have to re-derive the medoid/spread the class no longer carries the inputs for).
    let dampened_member_factor = 1.0 + mc.ln_1p();
    class.roi = class.roi / mc * dampened_member_factor;
}

/// Apply a [`CachedRefinement`] to a [`CandidateCloneClass`] in place (Plan 4a). Shared between
/// the warm (cache-hit) and cold (compute+store) paths of [`IndexDatabase::refine_class_in_place`].
///
/// `metrics_sampled` accumulates the sampling dimensions via OR-in:
/// - `refinement.lcs_sampled` — either LCS cost cap engaged: the member-count sample
///   (`LCS_MEMBER_SAMPLE`) OR the per-pair length proxy (`LCS_MAX_SEQ_TOKENS`). This bit is now
///   PERSISTED in `clone_refinements` (Fix 3, #215 Plan 4a round-2), so it survives a warm cache
///   hit — the long-sequence dimension is no longer lost on a hit the way it was when the bit was
///   compute-only.
/// - `class.member_count > MEMBER_VALUE_CAP` — an independent, cache-agnostic guard for the
///   REFINE-INPUT sampling dimension (P2d): `load_refine_members` truncates refine inputs to
///   `MEMBER_VALUE_CAP` (50), so a class above that cap DROPS members from the refine population —
///   including the 51..=64 range that the larger `LCS_MEMBER_SAMPLE` (64) align cap would miss. The
///   threshold is therefore the loader cap, not the align cap. Deterministic from the class's
///   member count regardless of cache hit/miss, so it flags the sample even for a row that predates
///   the persisted `lcs_sampled` bit (default 0 on an additively-migrated DB until recomputed).
///
/// [`CachedRefinement`]: crate::index::clones::refine::cache::CachedRefinement
/// [`IndexDatabase::refine_class_in_place`]: crate::index::IndexDatabase
pub(crate) fn apply_refinement(
    class: &mut CandidateCloneClass,
    refinement: crate::index::clones::refine::cache::CachedRefinement,
) {
    // Swap the ROI cohesion multiplier for `refactorability` on refined classes (Plan 4a). The
    // other factors are unchanged (cross-module spread × member count × medoid body tokens ×
    // load-bearing factor); `cohesion_min_pairwise` stays surfaced for transparency.
    //
    // Coverage gate (#256): a class whose anti-unification found almost NO shared structure has a
    // near-zero `anti_unify_coverage` and a degenerate template (`⟨m0⟩` — the whole body is one
    // metavar). Such a class is not a refactorable clone, yet `member_count × body_token_len`
    // alone floated several coverage-0.00 13-member test-helper "classes" to the very top after the
    // giant split. `refactorability_v2`'s ×0.70 below 0.5 was too weak to overcome the raw
    // member-count signal. So gate the REFINED ROI on coverage as a mutually-exclusive band
    // (mirrors the `refactorability_v2` style, score.rs): a strong penalty below 0.3, the milder
    // 0.70 between 0.3 and 0.5, none at/above 0.5. This touches ONLY the refined sort — the
    // un-refined (Plan-2) sort has no coverage and is left alone.
    let coverage_gate = coverage_roi_gate(refinement.anti_unify_coverage);
    class.roi = class.cross_module_spread as f64
        * class.member_count as f64
        * class.body_token_len_medoid as f64
        * class.roi_factors.load_bearing_factor
        * refinement.refactorability
        * coverage_gate;

    class.refined = true;
    class.class_kind = "refined_class";
    class.lcs_ratio = Some(refinement.lcs_ratio);
    class.confidence = Some(refinement.confidence.as_db_str().to_string());
    class.refactorability = Some(refinement.refactorability);
    class.refine_mode = Some(refinement.refine_mode.as_db_str());

    // Plan 4b anti-unification payload — surfaced on BOTH the warm (cache-hit) and cold
    // (compute+store) paths because both flow through this helper. The two JSON columns are parsed
    // back into `serde_json::Value`; `.ok()` degrades a malformed/legacy row to `None` rather than
    // failing the whole class.
    class.template = Some(refinement.template.clone());
    class.variation_points = serde_json::from_str(&refinement.variation_points_json).ok();
    class.proposed_signature = serde_json::from_str(&refinement.proposed_signature_json).ok();
    class.anti_unify_coverage = Some(refinement.anti_unify_coverage);

    // Fold the LCS sampling dimensions into the class's metrics_sampled flag (OR-in so any
    // already-sampled Plan-2 metric stays flagged):
    //   1. refinement.lcs_sampled — either cost cap (member-count sample or long-seq proxy), now
    //      PERSISTED (Fix 3) so it is honored on BOTH the cold compute AND the warm cache hit.
    //   2. member_count > MEMBER_VALUE_CAP — independent, cache-agnostic guard for the REFINE-INPUT
    //      sampling dimension (P2d): `load_refine_members` truncates inputs to `MEMBER_VALUE_CAP`
    //      (50), so a class with 51..=64 members DROPS ≥1 member from the refine population even
    //      though it is below `LCS_MEMBER_SAMPLE` (64). The threshold is therefore the loader cap
    //      `MEMBER_VALUE_CAP`, not `LCS_MEMBER_SAMPLE` — using the larger align cap would miss the
    //      51..=64 range that the loader already sampled. (`MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE`,
    //      so this strictly subsumes the old member-count guard.) Covers a pre-persisted-bit row
    //      whose stored flag defaults to 0.
    class.metrics_sampled |= refinement.lcs_sampled || class.member_count > MEMBER_VALUE_CAP;
}

/// Build the [`CloneCompleteness`] provenance block shared by `find_clones` and
/// `clones_for_symbol`. Only `min_similarity` (θ), `min_copies`, `truncated`,
/// `refine_budget_clamped`, `stale_members`, and the index freshness summary vary per call; the
/// rest are fixed Plan-2 policy constants. `clones_for_symbol` always passes
/// `refine_budget_clamped = false` (it has no class limit).
pub(crate) fn build_completeness(
    min_similarity: f64,
    min_copies: usize,
    truncated: bool,
    refine_budget_clamped: bool,
    stale_members: usize,
    freshness: String,
) -> CloneCompleteness {
    CloneCompleteness {
        normalizer_kind: "baseline",
        normalizer_version: NORM_VERSION,
        min_similarity,
        min_tokens: crate::index::clones::MIN_TOKENS as i64,
        min_copies,
        candidate_metric: "overlap_max_denominator",
        containment_metric: "overlap_min_denominator",
        generated_excluded: true,
        tests_excluded: false,
        same_file_policy: "included",
        index_freshness: freshness,
        oracle_coverage: "n/a_baseline_only",
        truncated,
        refine_budget_clamped,
        stale_members,
        // #232 closed the previously-advertised multi-language gaps: comments are now skipped,
        // string + boolean literals bucket multi-language, and TS function-valued declarators are
        // fingerprinted. #253 closed two more intra-language literal-bucketing recall gaps (Kotlin
        // boolean/null leaves, the C/C++ char value leaf) — see `NORM_VERSION` (now 3). No
        // clone-substrate gaps are currently advertised open.
        known_index_gaps: Vec::new(),
    }
}
