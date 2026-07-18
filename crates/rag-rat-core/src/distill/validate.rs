//! Mechanical status floors and provenance checks for distilled records (#703).
//!
//! Every function here is PURE and deterministic — no DB, no model. The floors are the guardrails
//! that keep a model's outcome/decision claims honest: the EFFECTIVE outcome status is computed
//! from mechanical facts (was it reverted? does a closing keyword link the fix? is there any fix
//! edge at all?) with fixed precedence, so the model can never over-claim "landed" on a thread that
//! has no landing evidence. Phase 1 computes the mechanical inputs (revert / closing-keyword /
//! fix-edge) and stores them raw; the LLM pass (#704) supplies `model_status` and the provenance
//! evidence, and the read layer (#705) applies [`effective_status`]. All of it is implemented and
//! tested here so the floor logic exists before either consumer does.

use rag_rat_papertrail::{FixEdgeSource, OutcomeStatus, ThreadShape};

// NOTE: the closing-keyword floor is NOT derived by re-scanning commit text here. Reproducing the
// reference grammar (provider-aware keyword sets incl. GitLab gerunds, project scoping, issue-vs-PR
// kind, URL forms) is exactly what the papertrail closer-minting tier already does — so the floor
// is derived in `extract` from the presence of a TEXT-tier closing edge the canonical parser
// minted.

// NOTE: the `revert_override` floor is computed in `extract` (a downstream landed commit whose body
// reverts one of the record's CURRENT fix SHAs), not here. It deliberately does NOT key on the fix
// commit itself being a `Revert` — intentional revert work that landed is `landed`, not `reverted`.

/// The `no-fix-edge` floor: no closing edge and no merge commit establish that anything landed.
pub(crate) fn no_fix_edge(fix_edge_source: FixEdgeSource) -> bool {
    fix_edge_source == FixEdgeSource::None
}

/// A commenter/author association that denotes a maintainer (repo owner, org member, or invited
/// collaborator). Case-insensitive; anything else (CONTRIBUTOR / NONE / bots / unknown) is not.
// Wired by the #704 LLM pass (decision-provenance floor over cited evidence); exercised now by the
// floor tests so the logic ships and is verified before its consumer exists.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_maintainer_association(association: Option<&str>) -> bool {
    matches!(
        association.map(str::to_ascii_uppercase).as_deref(),
        Some("OWNER" | "MEMBER" | "COLLABORATOR")
    )
}

/// The decision-provenance floor (#704): a distilled DECISION is provenance-verified only when at
/// least one of its cited evidence units was authored by a maintainer — a drive-by contributor's
/// "we should do X" is a proposal, not a decision the project made.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decision_provenance_verified(cited_evidence_associations: &[Option<String>]) -> bool {
    cited_evidence_associations.iter().any(|a| is_maintainer_association(a.as_deref()))
}

/// The outcome-claim floor (#704, claimed-vs-measured): a model claim of `landed` is verified only
/// when a mechanical fix edge or closing keyword corroborates it; any non-landed claim needs no
/// corroboration (a thread can be honestly `unclear` / `descoped` with no fix edge).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn outcome_claim_verified(
    model_status: OutcomeStatus,
    has_fix_edge: bool,
    has_closing_keyword: bool,
) -> bool {
    match model_status {
        OutcomeStatus::Landed => has_fix_edge || has_closing_keyword,
        _ => true,
    }
}

/// Inputs to the effective-status resolver: the mechanical floors plus the (optional) model status.
pub(crate) struct EffectiveStatusInputs {
    pub revert_override: bool,
    pub closing_keyword: bool,
    pub fix_edge_source: FixEdgeSource,
    pub model_status: Option<OutcomeStatus>,
}

/// Resolve the EFFECTIVE outcome status with fixed precedence: revert > closing-keyword >
/// no-fix-edge > model. A revert wins outright; a closing keyword forces `landed` (the fix
/// mechanically closed the work); with no fix edge the record can never be `landed` (a model
/// `landed`/absent collapses to `unclear`, while an honest `descoped`/`superseded` stands);
/// otherwise the model's status is authoritative, defaulting to `unclear` when the model is silent.
pub(crate) fn effective_status(inputs: &EffectiveStatusInputs) -> OutcomeStatus {
    if inputs.revert_override {
        return OutcomeStatus::Reverted;
    }
    if inputs.closing_keyword {
        return OutcomeStatus::Landed;
    }
    if no_fix_edge(inputs.fix_edge_source) {
        return match inputs.model_status {
            Some(OutcomeStatus::Landed) | None => OutcomeStatus::Unclear,
            Some(other) => other,
        };
    }
    inputs.model_status.unwrap_or(OutcomeStatus::Unclear)
}

/// Mechanical thread-shape classification from discussion structure. `Thin` = little discussion (a
/// short body and at most one comment); `ReviewStream` = review-dominated back-and-forth (review
/// events are at least half of a non-trivial comment set); otherwise `Investigation`.
pub(crate) fn classify_thread_shape(
    total_comments: usize,
    review_comments: usize,
    body_len: usize,
) -> ThreadShape {
    if total_comments <= 1 && body_len < 400 {
        ThreadShape::Thin
    } else if review_comments >= 2 && review_comments * 2 >= total_comments {
        ThreadShape::ReviewStream
    } else {
        ThreadShape::Investigation
    }
}

#[cfg(test)]
mod tests {
    use rag_rat_papertrail::{FixEdgeSource, OutcomeStatus, ThreadShape};

    use super::{
        EffectiveStatusInputs, classify_thread_shape, decision_provenance_verified,
        effective_status, is_maintainer_association, no_fix_edge, outcome_claim_verified,
    };

    #[test]
    fn no_fix_edge_floor_fires_only_on_none() {
        assert!(no_fix_edge(FixEdgeSource::None));
        assert!(!no_fix_edge(FixEdgeSource::Provider));
        assert!(!no_fix_edge(FixEdgeSource::Text));
    }

    #[test]
    fn maintainer_and_decision_provenance() {
        assert!(is_maintainer_association(Some("OWNER")));
        assert!(is_maintainer_association(Some("member")));
        assert!(is_maintainer_association(Some("Collaborator")));
        assert!(!is_maintainer_association(Some("CONTRIBUTOR")));
        assert!(!is_maintainer_association(Some("NONE")));
        assert!(!is_maintainer_association(None));
        // Synthesized positive: a maintainer-authored evidence unit verifies the decision.
        assert!(decision_provenance_verified(&[Some("NONE".into()), Some("MEMBER".into())]));
        // All drive-by → not verified.
        assert!(!decision_provenance_verified(&[Some("CONTRIBUTOR".into()), None]));
    }

    #[test]
    fn outcome_claim_needs_corroboration_only_for_landed() {
        assert!(outcome_claim_verified(OutcomeStatus::Landed, true, false));
        assert!(outcome_claim_verified(OutcomeStatus::Landed, false, true));
        assert!(!outcome_claim_verified(OutcomeStatus::Landed, false, false));
        // Non-landed claims are self-consistent without a fix edge.
        assert!(outcome_claim_verified(OutcomeStatus::Descoped, false, false));
        assert!(outcome_claim_verified(OutcomeStatus::Unclear, false, false));
    }

    #[test]
    fn effective_status_precedence_revert_beats_everything() {
        let status = effective_status(&EffectiveStatusInputs {
            revert_override: true,
            closing_keyword: true,
            fix_edge_source: FixEdgeSource::Provider,
            model_status: Some(OutcomeStatus::Landed),
        });
        assert_eq!(status, OutcomeStatus::Reverted);
    }

    #[test]
    fn effective_status_closing_keyword_forces_landed_over_model() {
        let status = effective_status(&EffectiveStatusInputs {
            revert_override: false,
            closing_keyword: true,
            fix_edge_source: FixEdgeSource::Text,
            model_status: Some(OutcomeStatus::Unclear),
        });
        assert_eq!(status, OutcomeStatus::Landed);
    }

    #[test]
    fn effective_status_no_fix_edge_cannot_be_landed() {
        // Model over-claims landed with no fix edge → collapses to unclear.
        assert_eq!(
            effective_status(&EffectiveStatusInputs {
                revert_override: false,
                closing_keyword: false,
                fix_edge_source: FixEdgeSource::None,
                model_status: Some(OutcomeStatus::Landed),
            }),
            OutcomeStatus::Unclear,
        );
        // An honest descoped stands.
        assert_eq!(
            effective_status(&EffectiveStatusInputs {
                revert_override: false,
                closing_keyword: false,
                fix_edge_source: FixEdgeSource::None,
                model_status: Some(OutcomeStatus::Descoped),
            }),
            OutcomeStatus::Descoped,
        );
    }

    #[test]
    fn effective_status_defers_to_model_when_a_fix_edge_exists() {
        assert_eq!(
            effective_status(&EffectiveStatusInputs {
                revert_override: false,
                closing_keyword: false,
                fix_edge_source: FixEdgeSource::Provider,
                model_status: Some(OutcomeStatus::Superseded),
            }),
            OutcomeStatus::Superseded,
        );
        // Silent model with a fix edge → unclear (not a fabricated landed).
        assert_eq!(
            effective_status(&EffectiveStatusInputs {
                revert_override: false,
                closing_keyword: false,
                fix_edge_source: FixEdgeSource::Provider,
                model_status: None,
            }),
            OutcomeStatus::Unclear,
        );
    }

    #[test]
    fn thread_shape_classification() {
        assert_eq!(classify_thread_shape(0, 0, 120), ThreadShape::Thin);
        assert_eq!(classify_thread_shape(6, 5, 2000), ThreadShape::ReviewStream);
        assert_eq!(classify_thread_shape(8, 1, 2000), ThreadShape::Investigation);
        // A long body with no comments is still worth distilling — not thin.
        assert_eq!(classify_thread_shape(0, 0, 5000), ThreadShape::Investigation);
    }
}
