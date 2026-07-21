//! Effective outcome-status resolution for distilled records (#703 floors, #705 read layer).
//!
//! These functions are PURE and deterministic — no DB, no model. The EFFECTIVE outcome status is
//! computed from mechanical facts (was it reverted? does a closing keyword link the fix? is there
//! any fix edge at all?) with fixed precedence, so a model's `landed` claim can never survive on a
//! thread with no landing evidence. The extraction pass (#703) stores the raw floors
//! (`revert_override`, `closing_keyword_floor`, `fix_edge_source`) and the LLM pass (#704) stores
//! `outcome_status_model`; the read layer (#705) resolves them here.
//!
//! This lives in `rag-rat-papertrail` — not the `rag-rat-core` extraction module — because the
//! resolver is READ-side: `rag-rat-query` and this crate's own record reader both apply it, and
//! `rag-rat-query` is above `rag-rat-papertrail` but below `rag-rat-core`. The persisted enums it
//! consumes ([`OutcomeStatus`] etc.) already live here.

use crate::{FixEdgeSource, OutcomeStatus};

/// The `no-fix-edge` floor: no closing edge and no merge commit establish that anything landed.
pub fn no_fix_edge(fix_edge_source: FixEdgeSource) -> bool {
    fix_edge_source == FixEdgeSource::None
}

/// Inputs to the effective-status resolver: the mechanical floors plus the (optional) model status.
pub struct EffectiveStatusInputs {
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
pub fn effective_status(inputs: &EffectiveStatusInputs) -> OutcomeStatus {
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

#[cfg(test)]
mod tests {
    use super::{EffectiveStatusInputs, effective_status, no_fix_edge};
    use crate::{FixEdgeSource, OutcomeStatus};

    #[test]
    fn no_fix_edge_floor_fires_only_on_none() {
        assert!(no_fix_edge(FixEdgeSource::None));
        assert!(!no_fix_edge(FixEdgeSource::Provider));
        assert!(!no_fix_edge(FixEdgeSource::Text));
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
}
