//! Mechanical status floors and provenance checks for distilled records (#703).
//!
//! Every function here is PURE and deterministic — no DB, no model. These are the WRITE-side
//! guardrails: the extraction pass stores the raw mechanical floors (revert / closing-keyword /
//! fix-edge) and the provenance verdicts, so a later model claim can be checked against them. The
//! EFFECTIVE outcome status is resolved READ-side, over those floors, in
//! `rag_rat_papertrail::effective_status` (#705) — it lives there because the read layer sits below
//! `rag-rat-core` and cannot reach into it.

use rag_rat_papertrail::{OutcomeStatus, ThreadShape};

// NOTE: the closing-keyword floor is NOT derived by re-scanning commit text here. Reproducing the
// reference grammar (provider-aware keyword sets incl. GitLab gerunds, project scoping, issue-vs-PR
// kind, URL forms) is exactly what the papertrail closer-minting tier already does — so the floor
// is derived in `extract` from the presence of a TEXT-tier closing edge the canonical parser
// minted.

// NOTE: the `revert_override` floor is computed in `extract` (a downstream landed commit whose body
// reverts one of the record's CURRENT fix SHAs), not here. It deliberately does NOT key on the fix
// commit itself being a `Revert` — intentional revert work that landed is `landed`, not `reverted`.

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
    use rag_rat_papertrail::{OutcomeStatus, ThreadShape};

    use super::{
        classify_thread_shape, decision_provenance_verified, is_maintainer_association,
        outcome_claim_verified,
    };

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
    fn thread_shape_classification() {
        assert_eq!(classify_thread_shape(0, 0, 120), ThreadShape::Thin);
        assert_eq!(classify_thread_shape(6, 5, 2000), ThreadShape::ReviewStream);
        assert_eq!(classify_thread_shape(8, 1, 2000), ThreadShape::Investigation);
        // A long body with no comments is still worth distilling — not thin.
        assert_eq!(classify_thread_shape(0, 0, 5000), ThreadShape::Investigation);
    }
}
