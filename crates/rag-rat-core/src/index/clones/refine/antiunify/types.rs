use std::collections::BTreeMap;

use super::super::score::Confidence;

/// Extraction role for a variation point — what kind of helper parameter the hole would become.
/// Persisted as a stable machine string via [`MetavarKind::as_db_str`] (the `extraction_role` field
/// mirrors it for legibility in the JSON contract).
///
/// `serde` serializes each variant to its `as_db_str` machine string (`value_param`,
/// `closure_param`, `type_param`, `gapped`) so the persisted `variation_points_json` is legible
/// without consulting the Rust enum (matches the `_json` column convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
        (*self).into()
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
    pub(super) aligned: Vec<bool>,
    /// `col_map[m][i]` = the token index in member `m`'s seq matched to anchor spine column `i`,
    /// or `None` when member `m` deletes anchor column `i`. `col_map[anchor_idx]` is the
    /// identity map. Outer index is the member ordinal (parallel to `members`).
    pub(super) col_map: Vec<Vec<Option<usize>>>,
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
    pub(super) member_inserts: Vec<BTreeMap<usize, Vec<usize>>>,
    /// Cumulative LCS-DP cells (`Σ |anchor|·|member|`) the star align actually charged against the
    /// shared [`CellBudget`]. [`anti_unify`] seeds its re-descent budget with this so the
    /// matched-statement re-descent CONTINUES from where the parent star-align left off (one
    /// budget across the whole per-class anti-unify), rather than restarting at a full budget.
    pub(super) spent_cells: u64,
}

/// A per-span metavar before recurrence collapse.
pub(super) struct RunMetavar {
    pub(super) lo: usize,
    pub(super) snapped_kind: &'static str,
    pub(super) per_member_values: Vec<String>,
    pub(super) kind: MetavarKind,
    pub(super) type_hint: Option<String>,
    pub(super) confidence: Confidence,
    /// Whether the differing-callee guard fired for this run (Fix 5). Threaded into the collapsed
    /// `VariationPoint.differing_callee`.
    pub(super) differing_callee: bool,
    /// The syntactic type context — the recovered `: T` annotation text at this occurrence column,
    /// or `None` when the hole has no nearby annotation (Fix 3, Codex round-7). Part of the
    /// [`collapse_recurring`] key so two occurrences with the same value tuple + role but
    /// DIFFERENT annotations do NOT collapse into one metavar (which would reuse one param
    /// across two distinct typed slots). NOT carried onto the final `VariationPoint` — it only
    /// disambiguates collapse.
    pub(super) type_context: Option<String>,
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
pub(super) enum EmittedSpan {
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
    pub(super) fn lo(&self) -> usize {
        match *self {
            EmittedSpan::Raw(lo, _)
            | EmittedSpan::Statement { lo, .. }
            | EmittedSpan::Classified { lo, .. } => lo,
        }
    }

    pub(super) fn hi(&self) -> usize {
        match *self {
            EmittedSpan::Raw(_, hi)
            | EmittedSpan::Statement { hi, .. }
            | EmittedSpan::Classified { hi, .. } => hi,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metavar_kind_db_str_matches_serde_for_all_variants() {
        for (kind, token) in [
            (MetavarKind::ValueParam, "value_param"),
            (MetavarKind::ClosureParam, "closure_param"),
            (MetavarKind::TypeParam, "type_param"),
            (MetavarKind::Gapped, "gapped"),
        ] {
            assert_eq!(kind.as_db_str(), token);
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(token.into())
            );
        }
    }
}
