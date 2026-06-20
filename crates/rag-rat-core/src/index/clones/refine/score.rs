//! Refactorability + confidence scoring for a refined clone class (#215 Plan 4a Task 4).
//!
//! Both are deliberately simple, monotone functions of the class LCS ratio (and, for confidence,
//! the Plan-2 pairwise-similarity floor). 4b can refine the formulas; the persisted-as-string
//! `Confidence` band and the (0,1]-clamped `refactorability` are the stable 4a contract.

/// Persisted confidence band for a refined clone class. Stored as a lower-case string in
/// `clone_refinements.confidence` via [`Confidence::as_db_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub(crate) fn as_db_str(&self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }

    /// Parse the persisted lower-case band back (cache read-through). An unknown value degrades to
    /// `Low` rather than erroring — a stale/foreign band should never crash a read.
    pub(crate) fn from_db_str(value: &str) -> Self {
        match value {
            "high" => Confidence::High,
            "medium" => Confidence::Medium,
            _ => Confidence::Low,
        }
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
        for c in [Confidence::High, Confidence::Medium, Confidence::Low] {
            assert_eq!(Confidence::from_db_str(c.as_db_str()), c);
        }
        // unknown → Low (never panics).
        assert_eq!(Confidence::from_db_str("bogus"), Confidence::Low);
    }
}
