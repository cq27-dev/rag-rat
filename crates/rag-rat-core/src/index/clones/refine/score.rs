//! Refactorability + confidence scoring for a refined clone class (#215 Plan 4a Task 4).
//!
//! Both are deliberately simple, monotone functions of the class LCS ratio (and, for confidence,
//! the Plan-2 pairwise-similarity floor). 4b can refine the formulas; the persisted-as-string
//! `Confidence` band and the (0,1]-clamped `refactorability` are the stable 4a contract.

/// Persisted confidence band for a refined clone class. Stored as a lower-case string in
/// `clone_refinements.confidence` via [`Confidence::as_db_str`].
///
/// `serde` serializes to the same lower-case band (`high`/`medium`/`low`) as
/// [`Confidence::as_db_str`] so the band reads identically whether it comes from the dedicated
/// column or the embedded `variation_points_json` / `proposed_signature_json` payloads.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, strum::EnumString, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub(crate) fn as_db_str(&self) -> &'static str {
        (*self).into()
    }

    /// Parse the persisted lower-case band back (cache read-through). An unknown value degrades to
    /// `Low` rather than erroring — a stale/foreign band should never crash a read.
    pub(crate) fn from_db_str(value: &str) -> Self {
        value.parse().unwrap_or(Confidence::Low)
    }
}

/// 4a refactorability: the class LCS ratio clamped to `(0, 1]`. The clamp keeps it a strictly
/// positive ROI multiplier — a degenerate 0.0 ratio would zero the whole ROI and bury an otherwise
/// load-bearing class, so we floor at [`f64::EPSILON`] and cap at 1.0.
pub(crate) fn refactorability_v1(lcs_ratio: f64) -> f64 {
    // `f64::EPSILON < 1.0` always holds, so `clamp` never hits its `min > max` panic path.
    lcs_ratio.clamp(f64::EPSILON, 1.0)
}

/// 4a confidence band from the class LCS ratio and the Plan-2 pairwise-similarity floor
/// (`similarity_min`): `High` only when BOTH are ≥ 0.9, `Low` when EITHER is < 0.7, else `Medium`.
pub(crate) fn confidence_v1(lcs_ratio: f64, similarity_min: f64) -> Confidence {
    if lcs_ratio >= 0.9 && similarity_min >= 0.9 {
        Confidence::High
    } else if lcs_ratio < 0.7 || similarity_min < 0.7 {
        Confidence::Low
    } else {
        Confidence::Medium
    }
}

// ── Plan-4b v2 scoring (metavar-profile-aware) ───────────────────────────────────────────────────

use super::antiunify::{MetavarKind, Template};

/// Metavar profile derived from the anti-unified template: counts by kind + coverage summary.
///
/// Consumed by [`confidence_v2`] and [`refactorability_v2`]; built by [`metavar_profile`].
#[derive(Debug, Clone)]
pub(crate) struct MetavarProfile {
    pub(crate) total: usize,
    pub(crate) value: usize,
    pub(crate) closure: usize,
    pub(crate) typ: usize,
    pub(crate) gapped: usize,
    /// `true` when any VP's EXPLICIT `differing_callee` flag is set — the differing-callee guard
    /// actually fired (a genuine differing callee/method-name, the Plan-3 SCIP seam). Read off the
    /// VP flag, NOT re-inferred from `(kind, confidence)`: a `ClosureParam` is also `Medium` for
    /// generic closure-ish subtrees (`binary_expression`, `field_expression`, …), so a
    /// band-derived heuristic would wrongly apply the call-resolution downgrade to non-callee
    /// diffs (Fix 5).
    pub(crate) differing_callee: bool,
    pub(crate) anti_unify_coverage: f64,
}

/// Derive a [`MetavarProfile`] from the anti-unified template.
pub(crate) fn metavar_profile(template: &Template) -> MetavarProfile {
    let mut value = 0usize;
    let mut closure = 0usize;
    let mut typ = 0usize;
    let mut gapped = 0usize;
    let mut differing_callee = false;

    for vp in &template.variation_points {
        match vp.kind {
            MetavarKind::ValueParam => value += 1,
            MetavarKind::ClosureParam => closure += 1,
            MetavarKind::TypeParam => typ += 1,
            MetavarKind::Gapped => gapped += 1,
        }
        // Read the EXPLICIT differing-callee flag the anti-unify guard set (Fix 5) — do NOT
        // re-infer it from `(kind, confidence)`, which misfires on Medium closure-ish subtrees.
        if vp.differing_callee {
            differing_callee = true;
        }
    }

    MetavarProfile {
        total: value + closure + typ + gapped,
        value,
        closure,
        typ,
        gapped,
        differing_callee,
        anti_unify_coverage: template.anti_unify_coverage,
    }
}

/// 4b confidence band: starts from [`confidence_v1`] then ONLY downgrades (never upgrades).
///
/// Downgrade triggers — each fires independently; the number of triggers that fire determines
/// how many bands the result is lowered from v1. The result is clamped to `Low` (never below).
///
/// 1. `gapped > 0` — indel present; extractability is uncertain.
/// 2. `differing_callee` — a callee differs; Plan-3 SCIP resolution needed for confidence.
/// 3. `(closure + typ) / total > 0.5` (when `total > 0`) — majority of VPs are non-value.
/// 4. `anti_unify_coverage < 0.7` — less than 70 % of the spine is fixed.
///
/// **Invariant:** `confidence_v2(r, s, p) ≤ confidence_v1(r, s)` for all inputs.
pub(crate) fn confidence_v2(
    lcs_ratio: f64,
    similarity_min: f64,
    profile: &MetavarProfile,
) -> Confidence {
    let base = confidence_v1(lcs_ratio, similarity_min);

    // Count how many downgrade triggers fire.
    let mut downgrades: u32 = 0;
    if profile.gapped > 0 {
        downgrades += 1;
    }
    if profile.differing_callee {
        downgrades += 1;
    }
    if profile.total > 0 && (profile.closure + profile.typ) as f64 / profile.total as f64 > 0.5 {
        downgrades += 1;
    }
    if profile.anti_unify_coverage < 0.7 {
        downgrades += 1;
    }

    // Map Confidence to an integer band (High=2, Medium=1, Low=0), apply downgrades, clamp.
    let band = match base {
        Confidence::High => 2i32,
        Confidence::Medium => 1,
        Confidence::Low => 0,
    };
    let result_band = (band - downgrades as i32).max(0);
    match result_band {
        2 => Confidence::High,
        1 => Confidence::Medium,
        _ => Confidence::Low,
    }
}

/// 4b refactorability: starts from [`refactorability_v1`] then multiplies by penalty factors
/// derived from the metavar profile. Always `≤ refactorability_v1(lcs_ratio)`.
///
/// Penalty factors (all in `(0, 1]`; all applicable factors are multiplied together):
/// - Many metavars: `total ≥ 10` → `0.70`; `total ≥ 5` → `0.85`.
/// - High non-value fraction: `(closure + typ) / total > 0.5` → `0.80`.
/// - Gapped metavars present → `0.75`.
/// - Differing callee present → `0.85`.
/// - Low coverage: `anti_unify_coverage < 0.5` → `0.70`.
///
/// **Invariant:** `refactorability_v2(r, p) ≤ refactorability_v1(r)` for all inputs.
pub(crate) fn refactorability_v2(lcs_ratio: f64, profile: &MetavarProfile) -> f64 {
    let base = refactorability_v1(lcs_ratio);

    let mut factor = 1.0f64;
    // Many-metavar penalty (mutually exclusive bands; take the worse one).
    if profile.total >= 10 {
        factor *= 0.70;
    } else if profile.total >= 5 {
        factor *= 0.85;
    }
    // High non-value fraction.
    if profile.total > 0 && (profile.closure + profile.typ) as f64 / profile.total as f64 > 0.5 {
        factor *= 0.80;
    }
    // Gapped.
    if profile.gapped > 0 {
        factor *= 0.75;
    }
    // Differing callee.
    if profile.differing_callee {
        factor *= 0.85;
    }
    // Low coverage.
    if profile.anti_unify_coverage < 0.5 {
        factor *= 0.70;
    }

    // factor ∈ (0, 1] → product is in (0, 1] → result ≤ base. Clamp for float safety.
    (base * factor).clamp(f64::EPSILON, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refactorability_clamps_to_unit_interval() {
        // 0.0 floors to EPSILON (strictly positive), not 0.0.
        assert!(refactorability_v1(0.0) > 0.0);
        assert_eq!(refactorability_v1(0.0), f64::EPSILON);
        // > 1.0 caps to 1.0.
        assert_eq!(refactorability_v1(1.5), 1.0);
        // in-range passes through.
        assert_eq!(refactorability_v1(0.83), 0.83);
    }

    #[test]
    fn confidence_bands() {
        assert_eq!(confidence_v1(0.95, 0.92), Confidence::High);
        // high lcs but a loose pairwise floor → not High.
        assert_eq!(confidence_v1(0.95, 0.80), Confidence::Medium);
        // either below 0.7 → Low.
        assert_eq!(confidence_v1(0.65, 0.99), Confidence::Low);
        assert_eq!(confidence_v1(0.99, 0.65), Confidence::Low);
        // both mid-band → Medium.
        assert_eq!(confidence_v1(0.80, 0.80), Confidence::Medium);
    }

    #[test]
    fn confidence_db_str_round_trips() {
        for (c, token) in
            [(Confidence::High, "high"), (Confidence::Medium, "medium"), (Confidence::Low, "low")]
        {
            assert_eq!(c.as_db_str(), token);
            assert_eq!(Confidence::from_db_str(c.as_db_str()), c);
        }
        // unknown → Low (never panics).
        assert_eq!(Confidence::from_db_str("bogus"), Confidence::Low);
    }

    // ── v2 scoring tests ─────────────────────────────────────────────────────────────────────────

    fn clean_profile() -> MetavarProfile {
        MetavarProfile {
            total: 1,
            value: 1,
            closure: 0,
            typ: 0,
            gapped: 0,
            differing_callee: false,
            anti_unify_coverage: 0.95,
        }
    }

    #[test]
    fn confidence_v2_never_upgrades_above_v1() {
        // Property: across many (lcs_ratio, similarity_min, profile) combinations, v2 ≤ v1.
        let confidence_ord = |c: Confidence| match c {
            Confidence::High => 2u32,
            Confidence::Medium => 1,
            Confidence::Low => 0,
        };
        let ratios = [0.0, 0.5, 0.65, 0.70, 0.80, 0.90, 0.95, 1.0];
        let profiles = [
            clean_profile(),
            MetavarProfile { gapped: 1, total: 2, value: 1, ..clean_profile() },
            MetavarProfile {
                differing_callee: true,
                closure: 1,
                total: 2,
                value: 1,
                ..clean_profile()
            },
            MetavarProfile { closure: 3, typ: 1, total: 4, value: 0, ..clean_profile() },
            MetavarProfile { anti_unify_coverage: 0.4, ..clean_profile() },
            MetavarProfile {
                total: 10,
                value: 5,
                closure: 3,
                typ: 2,
                gapped: 0,
                ..clean_profile()
            },
        ];
        for &r in &ratios {
            for &s in &ratios {
                for p in &profiles {
                    let v1 = confidence_v1(r, s);
                    let v2 = confidence_v2(r, s, p);
                    assert!(
                        confidence_ord(v2) <= confidence_ord(v1),
                        "v2 {:?} > v1 {:?} at lcs={r} sim={s}",
                        v2,
                        v1
                    );
                }
            }
        }
    }

    #[test]
    fn refactorability_v2_never_above_v1() {
        // Property: v2 ≤ v1 for all inputs.
        let ratios = [0.0, 0.3, 0.5, 0.7, 0.85, 0.9, 0.95, 1.0, 1.5];
        let profiles = [
            clean_profile(),
            MetavarProfile { gapped: 2, total: 3, value: 1, ..clean_profile() },
            MetavarProfile {
                differing_callee: true,
                closure: 1,
                total: 2,
                value: 1,
                ..clean_profile()
            },
            MetavarProfile { total: 12, value: 2, closure: 8, typ: 2, ..clean_profile() },
            MetavarProfile { anti_unify_coverage: 0.3, ..clean_profile() },
        ];
        for &r in &ratios {
            for p in &profiles {
                let v1 = refactorability_v1(r);
                let v2 = refactorability_v2(r, p);
                assert!(v2 <= v1 + 1e-12, "v2 {v2} > v1 {v1} at lcs={r}");
            }
        }
    }

    #[test]
    fn differing_callee_class_bands_at_most_medium() {
        // A class with a differing callee must band at most Medium, even with perfect LCS/sim.
        let profile = MetavarProfile {
            total: 1,
            value: 0,
            closure: 1,
            typ: 0,
            gapped: 0,
            differing_callee: true,
            anti_unify_coverage: 0.95,
        };
        let result = confidence_v2(0.99, 0.99, &profile);
        assert!(
            matches!(result, Confidence::Medium | Confidence::Low),
            "differing_callee class must band ≤ Medium, got {:?}",
            result
        );
    }
}
