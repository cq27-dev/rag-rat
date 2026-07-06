//! Public result / options / eligibility types for the clone query API (#215 Plan 2 + Plan 4).
//!
//! These are the DTOs that cross the MCP/CLI serde boundary: the ranked clone class
//! ([`CandidateCloneClass`]) with its ROI factors ([`RoiFactors`]) and members ([`CloneMember`]),
//! the provenance block ([`CloneCompleteness`]), the eligibility verdict
//! ([`CloneEligibility`] / [`CloneIneligibilityReason`]), and the `find_clones` /
//! `clones_for_symbol` option + result shapes. The algorithm that produces them lives in the
//! sibling `substrate` / `build` / `scoring` / `resolve` modules; the orchestration on
//! `IndexDatabase` is in the module index (`mod.rs`).

#[derive(Debug, Clone, serde::Serialize)]
pub struct CloneMember {
    pub r#ref: String, // qualified name "path::symbol"
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub token_len: i64,
    pub language: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RoiFactors {
    pub member_count: usize,
    pub cross_module_spread: usize,
    pub median_token_len: i64,
    pub load_bearing_factor: f64,
    pub cohesion_penalty: f64, // = cohesion_min_pairwise (the multiplier applied)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CandidateCloneClass {
    pub class_key: String,        // read-side key (NOT clone_refinements key)
    pub class_kind: &'static str, // always "candidate_component" in Plan 2
    pub language: String,
    pub refined: bool,             // always false in Plan 2
    pub members: Vec<CloneMember>, // returned subset (capped)
    pub member_count: usize,
    pub members_returned: usize,
    pub total_members: usize,
    pub similarity_min: f64,        // min pairwise overlap/max_len
    pub similarity_medoid_min: f64, // min similarity of any member to the medoid
    pub containment_max: f64,       // max pairwise overlap/min_len (informational)
    pub cohesion_min_pairwise: f64, // == similarity_min; the ROI cohesion input
    pub cross_module_spread: usize,
    pub body_token_len_medoid: i64,
    pub roi: f64,
    pub roi_factors: RoiFactors,
    /// `true` when a cost cap engaged for this class's metric computation, so a sampled metric is
    /// distinguishable from an exact one. Two independent caps set it:
    /// - The Plan-2 pairwise-metric cap
    ///   ([`METRIC_SAMPLE_CAP`](super::substrate::METRIC_SAMPLE_CAP)): the component has more than
    ///   `METRIC_SAMPLE_CAP` members and the pairwise loop (`similarity_min`, medoid,
    ///   `similarity_medoid_min`, `containment_max`) ran over only the first `METRIC_SAMPLE_CAP`.
    /// - The Plan-4a refine LCS caps (`LCS_MEMBER_SAMPLE` / `LCS_MAX_SEQ_TOKENS`, Fix 1): the
    ///   refine pass bounded the all-pairs LCS by sampling members and/or replacing very long
    ///   pairs with the multiset-Dice proxy. `refine_class_in_place` ORs the refinement's
    ///   `lcs_sampled` into this flag, so a refined class with a cost-bounded fidelity reports
    ///   `true`.
    ///
    /// `member_count` / `total_members` / `class_key` are always over the FULL component.
    /// `false` for all normal-size components computed exactly (the typical case).
    pub metrics_sampled: bool,
    /// The `symbol_id` (`symbols.id` rowid) of the **medoid member** — the member that maximises
    /// the sum of pairwise bag-overlap similarities within the metric-sampled subset of the class.
    ///
    /// **Caveat (Plan 4b §1.1):** this is the *bag-overlap* medoid (max Σ overlap/max_len), NOT an
    /// LCS-distance medoid. For a coherence-split class (all pairs ≥ θ) it is a sound,
    /// deterministic template-spine anchor for anti-unification; the distinction is documented
    /// and harmless.
    ///
    /// **Metrics-sampled note:** when [`metrics_sampled`] is `true`, the medoid is selected over
    /// the first [`METRIC_SAMPLE_CAP`](super::substrate::METRIC_SAMPLE_CAP) members (id-ASC stable
    /// order), not the full component. The resolved `symbol_id` is still a real member's id
    /// and is a valid anchor; only the coverage of the medoid search is reduced. Task 5d falls
    /// back to the canonical-first `(struct_hash, path, start_byte)` member when this field is
    /// `None`.
    ///
    /// `None` only in the degenerate case where `metric_bags` is empty (a component with zero
    /// members that passed the size filter — should not occur in practice).
    ///
    /// Codex #5: NOT serialized (`#[serde(skip)]`). This is a `symbols.id` ROWID, reassigned on
    /// every reindex — the API contract is stable refs / `sym_<hex>` handles, never raw
    /// rowids. It is INTERNAL state, threaded only into the anti-unify anchor
    /// (`resolve_anchor_idx`); consumers (MCP / CLI / clients) don't need it and must not be
    /// tempted to cache it across a rebuild.
    ///
    /// [`metrics_sampled`]: Self::metrics_sampled
    #[serde(skip)]
    pub medoid_symbol_id: Option<i64>,
    /// Refinement outputs (#215 Plan 4a). All `None` on an UN-refined candidate class; populated
    /// only on classes the two-phase driver refined (`refined == true`). `lcs_ratio` is the NiCad
    /// class fidelity (min pairwise `2·LCS/(|a|+|b|)`); `confidence` is the persisted band
    /// (`"high"`/`"medium"`/`"low"`); `refactorability` is the `(0,1]`-clamped ROI multiplier;
    /// `refine_mode` is `Some("baseline")` when refined.
    pub lcs_ratio: Option<f64>,
    pub confidence: Option<String>,
    pub refactorability: Option<f64>,
    pub refine_mode: Option<&'static str>,
    /// Anti-unification template text (Plan 4b): fixed runs verbatim, variation runs as `⟨m0⟩`,
    /// gapped runs as `⟨m2?⟩`. `None` on un-refined classes.
    pub template: Option<String>,
    /// Variation points parsed from `variation_points_json`. Each point has `metavar_id`, `kind`,
    /// `occurrences`, `per_member_values`, `extraction_role`, `type_hint`, `confidence`. The
    /// `per_member_values` array is ordinal-aligned to the canonical `(struct_hash, path,
    /// start_byte)` sorted member order (the `load_refine_members` basis) — NOT to the `members`
    /// field above, which `build_class` emits in `symbol_id` order. `None` on un-refined classes.
    pub variation_points: Option<serde_json::Value>,
    /// Proposed signature parsed from `proposed_signature_json` (Plan 4b): `params`, `typedness`,
    /// `confidence`, `text`, `unresolved_type_slots`, `return_type`. `None` on un-refined classes.
    pub proposed_signature: Option<serde_json::Value>,
    /// Real anti-unify coverage (`fixed_spine_columns / total_spine_columns` ∈ [0,1]; `1.0` when
    /// all members are structurally identical). Distinct from `lcs_ratio` (NiCad class fidelity).
    /// `None` on un-refined classes.
    pub anti_unify_coverage: Option<f64>,
    /// LOCATION-BEARING member identities (`ref@path:start-end`) in the canonical
    /// `(struct_hash, path, start_byte)` order — capped at the same `MEMBER_VALUE_CAP` — that
    /// `load_refine_members` aligns `per_member_values` to. ORDINAL-ALIGNED to
    /// `variation_points[*].per_member_values`, so a consumer (`clones --explain`, MCP output) can
    /// map each per-member value back to a UNIQUE member. Codex #4: the identity is
    /// location-bearing (the SAME shape `class_key` uses), not the bare qualified `ref` — a
    /// class with duplicate refs (overloads / same-named methods in one file) would otherwise
    /// label values indistinguishably. Distinct from `members` (which is `r#ref`-sorted
    /// display order). Always populated by `build_class` (cheap, no re-parse) so it is present
    /// on refined classes regardless of warm/cold refine path; `None` only on the degenerate
    /// stub paths that never run `build_class`.
    pub canonical_member_refs: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CloneCompleteness {
    pub normalizer_kind: &'static str, // "baseline"
    pub normalizer_version: i64,
    pub min_similarity: f64,              // θ used
    pub min_tokens: i64,                  // MIN_TOKENS
    pub min_copies: usize,                // member-count floor applied
    pub candidate_metric: &'static str,   // "overlap_max_denominator"
    pub containment_metric: &'static str, // "overlap_min_denominator"
    pub generated_excluded: bool,         // true
    pub tests_excluded: bool,             // current policy
    pub same_file_policy: &'static str,   // "included" (Plan 2 keeps same-file pairs)
    pub index_freshness: String,          // reuse index_status freshness summary
    pub oracle_coverage: &'static str,    // "n/a_baseline_only" in Plan 2 (SCIP is Plan 3)
    pub truncated: bool,
    /// `true` when `limit` was supplied and was clamped by the refine budget (the budget,
    /// currently 50, is less than the requested limit AND the total built classes exceed the
    /// budget). A `true` value means classes beyond the refine budget were dropped — use
    /// `limit: None` to retrieve all classes. Always `false` on the unlimited path and on
    /// `clones_for_symbol`.
    pub refine_budget_clamped: bool,
    /// Count of DISTINCT member file paths whose on-disk content no longer matches the indexed
    /// `files.sha256`. A non-zero value means the returned clone classes describe STALE file
    /// contents — consumers should reindex / `rag-rat heal` before acting on these results.
    /// This is a read-only signal; Plan 2 does not heal-before-return.
    pub stale_members: usize,
    /// Advertised-open clone-substrate gaps (a deliberately self-honest provenance signal). Empty
    /// since #232 closed the multi-language gaps (comments skipped, string/boolean literals
    /// bucketed, TS function-valued declarators fingerprinted); a future known limitation goes
    /// here.
    pub known_index_gaps: Vec<String>,
}

/// Why a *resolved* symbol is not clone-eligible (#274 item 3a). The `symbol_fingerprinted = false`
/// bool conflated several distinct causes — this names them so a consumer can tell "below
/// `MIN_TOKENS`" (a near-miss worth nothing) from "the file is generated" (excluded by policy) from
/// "the index is stale at this symbol" (a reindex would fix it). It is a closed, persisted-style
/// enum: [`as_db_str`](Self::as_db_str) / [`from_db_str`](Self::from_db_str) give the stable wire
/// token that crosses the MCP/CLI serde boundary, and `serde` emits that same snake_case token.
///
/// Reasons are determined in PRIORITY order (see [`classify_ineligibility_reason`]), so exactly one
/// is reported even when several apply (a generated file ALSO has a non-function `_` symbol, etc.):
/// `Generated` ⊃ `StaleNormalizerVersion` ⊃ `NonFunctionKind` ⊃ `BelowMinTokens`.
///
/// [`classify_ineligibility_reason`]: super::resolve::classify_ineligibility_reason
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, strum::EnumString, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CloneIneligibilityReason {
    /// The symbol's file is generated (`files.generated = 1` — a `kind = generated` target or a
    /// path-heuristic codegen file like `src/generated/…`/`.d.ts`). Generated code is excluded from
    /// clone recall by policy; checked FIRST because a generated file's symbols are never
    /// fingerprinted regardless of kind/size.
    Generated,
    /// The symbol is not a function-shaped declaration: its `kind` is neither `"function"` nor a
    /// function-valued declarator (`const f = () => …`, a class-field arrow — #232 #5). Only
    /// function-shaped symbols are fingerprinted, so this can never be a clone-class member.
    NonFunctionKind,
    /// A baseline fingerprint row EXISTS for this symbol but at a `normalizer_version` other than
    /// the current [`NORM_VERSION`] — the index was last fingerprinted by an older binary and
    /// the read filter excludes it. A `rag-rat index --full` with the current binary fixes it.
    /// Distinct from `BelowMinTokens` (where NO row exists at all): here the symbol WAS
    /// eligible, the index is just stale.
    ///
    /// [`NORM_VERSION`]: crate::index::clones::NORM_VERSION
    StaleNormalizerVersion,
    /// The symbol is function-shaped and in a non-generated file, but no current-version
    /// fingerprint row exists — its body normalized below [`MIN_TOKENS`](crate::index::clones)
    /// tokens (the size floor), so it was never fingerprinted. The residual catch-all once
    /// `Generated` / `NonFunctionKind` / `StaleNormalizerVersion` are ruled out.
    BelowMinTokens,
}

impl CloneIneligibilityReason {
    /// The stable wire/DB token for this reason (matches the `serde` snake_case rename).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a wire/DB token back into a reason (inverse of [`as_db_str`](Self::as_db_str)).
    pub fn from_db_str(value: &str) -> Option<Self> {
        value.parse().ok()
    }
}

/// The clone-eligibility verdict for a
/// [`clones_for_symbol`](crate::index::IndexDatabase::clones_for_symbol) selector — the richer
/// companion to the `symbol_resolved` / `symbol_fingerprinted` bools (#274 item 3a). Serializes as
/// an internally-tagged object (`{ "status": "eligible" }`, `{ "status": "ineligible", "reason":
/// "below_min_tokens" }`, `{ "status": "symbol_not_resolved" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CloneEligibility {
    /// The selector matched no scoped symbol (`symbol_resolved = false`).
    SymbolNotResolved,
    /// The symbol resolved AND carries a current-version baseline fingerprint (clone-eligible).
    Eligible,
    /// The symbol resolved but is not clone-eligible; `reason` says why.
    Ineligible { reason: CloneIneligibilityReason },
}

/// Result of [`IndexDatabase::clones_for_symbol`]. Carries eligibility flags + a completeness block
/// so a caller can distinguish "selector matched nothing" from "matched a symbol that is not
/// eligible for fingerprinting" from "eligible but unique (in no clone class)".
///
/// [`IndexDatabase::clones_for_symbol`]: crate::index::IndexDatabase::clones_for_symbol
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClonesForSymbolResult {
    /// The containing candidate clone class, or `None` when the symbol is in no class (unique, not
    /// eligible, or unresolved).
    pub class: Option<CandidateCloneClass>,
    /// The selector matched a scoped symbol.
    pub symbol_resolved: bool,
    /// That symbol has a current-version baseline fingerprint loaded into the candidate set
    /// (eligible: a `kind="function"` symbol OR a function-valued declarator — `const f = () =>
    /// …`, a class-field arrow handler, #232 #5 — ≥ `MIN_TOKENS` in a non-generated, in-scope
    /// file).
    pub symbol_fingerprinted: bool,
    /// The richer eligibility verdict (#274 item 3a): when `symbol_fingerprinted = false` this
    /// names WHY the symbol is not clone-eligible (generated file / non-function kind / stale
    /// normalizer_version / below `MIN_TOKENS`) instead of conflating all four into one bool.
    /// Stays consistent with the bools: `Eligible` ⇔ `symbol_fingerprinted`,
    /// `SymbolNotResolved` ⇔ `!symbol_resolved`, `Ineligible { .. }` ⇔ `symbol_resolved &&
    /// !symbol_fingerprinted`.
    pub eligibility: CloneEligibility,
    /// Same provenance block as [`FindClonesResult::completeness`].
    pub completeness: CloneCompleteness,
}

/// Options for [`IndexDatabase::find_clones`].
///
/// [`IndexDatabase::find_clones`]: crate::index::IndexDatabase::find_clones
#[derive(Debug, Clone)]
pub struct FindClonesOptions {
    /// Minimum similarity threshold (overlap/max_len). Defaults to [`THETA`](super::THETA) if
    /// `None`.
    pub min_similarity: Option<f64>,
    /// Minimum number of copies for a class to be returned. Defaults to 2 if `None`.
    pub min_copies: Option<usize>,
    /// Maximum number of classes to return (sorted by ROI desc). No limit if `None`. Note: a
    /// limited query is clamped to the refine budget (currently 50) — `limit: Some(N)` returns at
    /// most 50 classes, all refined. To retrieve more classes (only the top 50 refined), use
    /// `limit: None`.
    pub limit: Option<usize>,
}

/// Result of [`IndexDatabase::find_clones`].
///
/// [`IndexDatabase::find_clones`]: crate::index::IndexDatabase::find_clones
#[derive(Debug, Clone, serde::Serialize)]
pub struct FindClonesResult {
    pub classes: Vec<CandidateCloneClass>,
    pub completeness: CloneCompleteness,
}

/// How to identify the subject symbol for [`IndexDatabase::clones_for_symbol`].
///
/// [`IndexDatabase::clones_for_symbol`]: crate::index::IndexDatabase::clones_for_symbol
#[derive(Debug, Clone)]
pub enum CloneSymbolSelector {
    /// An opaque `sym_<hex>` logical-symbol handle (as emitted by symbol-returning tools).
    Id(String),
    /// A fully-qualified `"path/to/file.rs::symbol_name"` reference.
    Ref(String),
    /// The tightest-spanning in-scope symbol whose line range contains `line` in `path`.
    PathLine { path: String, line: i64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_ineligibility_reason_db_str_matches_serde_for_all_variants() {
        for (reason, token) in [
            (CloneIneligibilityReason::Generated, "generated"),
            (CloneIneligibilityReason::NonFunctionKind, "non_function_kind"),
            (CloneIneligibilityReason::StaleNormalizerVersion, "stale_normalizer_version"),
            (CloneIneligibilityReason::BelowMinTokens, "below_min_tokens"),
        ] {
            assert_eq!(reason.as_db_str(), token);
            assert_eq!(CloneIneligibilityReason::from_db_str(reason.as_db_str()), Some(reason));
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::String(token.into())
            );
        }
        assert_eq!(CloneIneligibilityReason::from_db_str("bogus"), None);
    }
}
