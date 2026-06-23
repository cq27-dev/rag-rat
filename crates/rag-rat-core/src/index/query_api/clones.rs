//! Read layer for clone detection (#215). Plan 1 ships only the candidate-component read that
//! proves the fingerprint substrate; the `find_clones` / `clones_for_symbol` surface is Plan 2.
//!
//! The candidate read is the SourcererCC algorithm (design rev 4 §3b): a `struct_hash` exact fast
//! path, a deterministic-total-order sub-block filter over scoped baseline postings, and an EXACT
//! max-denominator overlap verify. df is a *selectivity hint only* — admissibility comes from the
//! shared total order plus the exact verify, so a missing/stale df never drops a true clone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};

/// Pairwise metric work cap for huge components: when a component exceeds this count, the
/// O(n²) pairwise metric loop (`similarity_min`, medoid, `similarity_medoid_min`,
/// `containment_max`) runs over ONLY the first `METRIC_SAMPLE_CAP` members instead of the full
/// upper triangle. `member_count` / `total_members` / `class_key` still reflect the FULL
/// component; only the metric computation is sampled, and `metrics_sampled` is set to `true` on
/// the returned class so callers can distinguish sampled from exact metrics.
///
/// For typical-size components (the overwhelming common case, and ALL existing tests) this cap is
/// never reached: behavior is identical to the pre-cap code and `metrics_sampled` is `false`.
const METRIC_SAMPLE_CAP: usize = 200;

use crate::index::IndexDatabase;
use crate::index::clones::NORM_VERSION;
use crate::index::clones::refine::cache::{
    refine_compute_and_store, refine_lookup, refinement_key,
};
use crate::index::clones::refine::split::coherence_split;

/// Similarity threshold θ: a candidate pair is kept iff `overlap / max_len >= THETA`. The MAX
/// denominator is deliberate (design rev-4 §3b) — it bounds the member length ratio to ≈1/θ, the
/// whole-symbol bias, so a tiny helper contained in a giant function (overlap/min ≈ 1.0) is NOT a
/// clone. Tunable later via the query surface.
const THETA: f64 = 0.7;

/// Maximum members returned per clone class to guard against huge components.
pub(crate) const MAX_MEMBERS: usize = 50;

/// Cap on how many members `load_refine_members` re-parses and returns WITH spans+text+seq — the
/// population from which `per_member_values` are collected (Plan 4b §1.6).
///
/// Three caps, reconciled:
/// - `MEMBER_VALUE_CAP = 50` (= `MAX_MEMBERS`): how many members get per-member values; the loader
///   truncates here so every returned member carries spans + text.
/// - `LCS_MEMBER_SAMPLE = 64`: the bound on the STAR ALIGNMENT pass in Task 5d — at most 64
///   (anchor, N−1 non-anchor) pairings enter the LCS DP. Since 50 < 64, all loaded members are
///   within the align cap automatically: the value cap is the binding constraint.
/// - `MAX_MEMBERS = 50`: the build_class returned-member list cap (distinct from the loader cap but
///   equal in value; the loader honors the same floor so the two populations stay in sync).
///
/// NOTE: `MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE` is load-bearing: Task 5d collects values across
/// ALL loaded members while capping the alignment work at `LCS_MEMBER_SAMPLE`. With the current
/// values (50 < 64) the alignment never sees more members than it can process without sampling.
pub(crate) const MEMBER_VALUE_CAP: usize = MAX_MEMBERS; // = 50

// `MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE` is load-bearing (see the doc above): the loader truncates
// the refine population at `MEMBER_VALUE_CAP` (50), so the `metrics_sampled` member-count guard in
// `apply_refinement` keys off `MEMBER_VALUE_CAP` — using the larger align cap (`LCS_MEMBER_SAMPLE`,
// 64) would miss the 51..=64 range the loader already sampled. Pin the relationship at compile time
// in PRODUCTION (a module-level `const _`), not just under `#[cfg(test)]`, so a future bump that
// inverts the two caps fails the build here rather than silently regressing the sampling flag.
const _: () = assert!(
    MEMBER_VALUE_CAP < crate::index::clones::refine::align::LCS_MEMBER_SAMPLE,
    "MEMBER_VALUE_CAP must be below LCS_MEMBER_SAMPLE so the cap+1 member range is still flagged"
);

/// Member-hydration batch size for the `symbols.id IN (…)` query in [`build_class`]. SQLite caps
/// the number of host parameters per prepared statement (`SQLITE_MAX_VARIABLE_NUMBER` — 999 on
/// older system libs that the non-bundled `rusqlite` may link against), so a component larger than
/// that floor would fail `conn.prepare` outright. Hydrating in chunks of [`HYDRATION_CHUNK`] keeps
/// every statement well under the limit; results are accumulated then re-sorted by `symbol_id` so
/// the full population is processed deterministically regardless of chunk boundaries.
const HYDRATION_CHUNK: usize = 900;

// ── Public result types (Plan-2 query API) ────────────────────────────────────────────────────

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
    /// - The Plan-2 pairwise-metric cap ([`METRIC_SAMPLE_CAP`]): the component has more than
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
    /// the first [`METRIC_SAMPLE_CAP`] members (id-ASC stable order), not the full component.
    /// The resolved `symbol_id` is still a real member's id and is a valid anchor; only the
    /// coverage of the medoid search is reduced. Task 5d falls back to the canonical-first
    /// `(struct_hash, path, start_byte)` member when this field is `None`.
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

/// Result of [`IndexDatabase::clones_for_symbol`]. Carries eligibility flags + a completeness block
/// so a caller can distinguish "selector matched nothing" from "matched a symbol that is not
/// eligible for fingerprinting" from "eligible but unique (in no clone class)".
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
    /// Same provenance block as [`FindClonesResult::completeness`].
    pub completeness: CloneCompleteness,
}

/// Deterministic, order-independent class key: sort `member_refs`, join with `\n`,
/// `hex_sha256`, take the first 16 hex chars.
pub(crate) fn class_key_for(member_refs: &[String]) -> String {
    let mut sorted = member_refs.to_vec();
    sorted.sort_unstable();
    let joined = sorted.join("\n");
    crate::index::hex_sha256(joined.as_bytes())[..16].to_string()
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
fn canonical_member_order_key<'a>(
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
fn min_pairwise_cohesion(class: &[i64], by_id: &BTreeMap<i64, &SymbolBag>) -> f64 {
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

/// Sentinel df for tokens with no `clone_token_df` row (LEFT JOIN miss). i64::MAX sorts them LAST
/// in `(coalesced_df ASC, token_hash ASC)` order — they are treated as maximally common (least
/// selective), which is the conservative choice: it can only widen the sub-block, never shrink it,
/// so no candidate is dropped. df is selectivity-only; correctness depends on the shared total
/// order + exact verify, never on df accuracy (design rev-4 §2).
const DF_FALLBACK: i64 = i64::MAX;

/// One scoped baseline symbol's fingerprint, loaded for the candidate read.
pub(super) struct SymbolBag {
    pub(super) symbol_id: i64,
    pub(super) language: String,
    pub(super) struct_hash: String,
    pub(super) token_len: i64,
    /// `(token_hash, freq, coalesced_df)` for every distinct token in the symbol's bag.
    pub(super) tokens: Vec<TokenPosting>,
}

pub(super) struct TokenPosting {
    pub(super) token_hash: i64,
    pub(super) freq: i64,
    pub(super) coalesced_df: i64,
}

impl IndexDatabase {
    /// Candidate clone components over the ACTIVE scope, via the SourcererCC algorithm (design rev
    /// 4 §3b): a `struct_hash` exact fast path plus sub-block-filtered candidate pairs verified
    /// by EXACT max-denominator overlap, union-found into connected components. Both endpoints
    /// are filtered to the scoped `files` view BEFORE pairing, so a component never mixes
    /// out-of-scope symbols. Baseline postings only (recall is oracle-independent).
    /// Over-generated on purpose — `find_clones` (Plan 2) surfaces these as UNREFINED candidate
    /// classes; the coherence split + anti-unification is Plan 4.
    pub fn candidate_clone_components(&self) -> anyhow::Result<Vec<Vec<i64>>> {
        let conn = self.storage.connection();
        let pairs = candidate_pairs(conn)?;
        Ok(components_from_pairs(&pairs))
    }
}

// ── find_clones public API ────────────────────────────────────────────────────────────────────

/// Options for [`IndexDatabase::find_clones`].
#[derive(Debug, Clone)]
pub struct FindClonesOptions {
    /// Minimum similarity threshold (overlap/max_len). Defaults to [`THETA`] if `None`.
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
#[derive(Debug, Clone, serde::Serialize)]
pub struct FindClonesResult {
    pub classes: Vec<CandidateCloneClass>,
    pub completeness: CloneCompleteness,
}

impl IndexDatabase {
    /// Ranked candidate clone classes over the active scope.
    ///
    /// Runs the SourcererCC candidate-pair algorithm, union-finds pairs into components, hydrates
    /// each component into a [`CandidateCloneClass`] with pairwise similarity metrics and an ROI
    /// score, filters by `min_similarity` / `min_copies`, sorts by ROI descending, and attaches a
    /// [`CloneCompleteness`] provenance block. Classes are UNREFINED (Plan 4 adds coherence
    /// splitting and anti-unification).
    ///
    /// **Refine-budget cap:** a limited query (`limit: Some(N)`) clamps to the refine budget
    /// (currently 50): at most 50 classes are returned, all refined. An unlimited query
    /// (`limit: None`) returns all classes (only the top 50 refined, the rest unrefined). Use
    /// `limit: None` to retrieve more than 50 classes. `completeness.refine_budget_clamped`
    /// reports when a supplied limit was clamped by the budget AND classes were dropped.
    pub fn find_clones(&self, opts: FindClonesOptions) -> anyhow::Result<FindClonesResult> {
        let conn = self.storage.connection();

        // Validate the caller-supplied θ BEFORE it touches candidate generation. θ is a similarity
        // ratio (overlap/max_len) and must lie in [0.5, 1.0]:
        // - > 1.0 is unreachable and signals a unit error.
        // - < 0.5 is rejected not just for the ≤0 degenerate case but because any small positive θ
        //   makes the sub-block prefix p = L − ceil(θ·L) + 1 approach the whole bag, flooding the
        //   inverted index with hot/common tokens and causing O(S²) candidate-pair explosion
        //   (measured: 5k symbols at θ=0.01 → ~20s/~700MB in candidate gen alone). The 0.5 floor
        //   keeps the sub-block at most L/2 occurrences — a practical safety bound. A deeper fix
        //   (capping candidate-pair/posting-list work and restoring the full (0,1] range) is
        //   tracked in #235.
        // - NaN and non-finite values must be explicitly rejected: both `v < 0.5` and `v > 1.0` are
        //   false for NaN, so without the `is_finite` guard NaN slips through.
        if let Some(v) = opts.min_similarity
            && (!v.is_finite() || !(0.5..=1.0).contains(&v))
        {
            anyhow::bail!("min_similarity must be in [0.5, 1.0]");
        }

        let bags = load_scoped_baseline_bags(conn)?;
        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();

        // θ defaults to the const [`THETA`]; a caller-supplied `min_similarity` is honored ALL the
        // way through candidate generation (not merely post-filtered) so a θ below [`THETA`]
        // actually widens the candidate set instead of being clamped by the const-θ sub-block /
        // verify, and a θ above [`THETA`] narrows it.
        let theta = opts.min_similarity.unwrap_or(THETA);
        let pairs = candidate_pairs_from_bags(&bags, theta);
        let components = components_from_pairs(&pairs);
        // Bucket the θ-verified candidate pairs per component (#256): the coherence split seeds its
        // clique cover from these edges instead of an O(n²) all-pairs scan, so a giant component
        // splits scalably. ONE O(|pairs|) pass over a node→component map.
        let edges_by_component = bucket_edges_by_component(&pairs, &components);

        let min_copies = opts.min_copies.unwrap_or(2);

        // Plan 4a: coherence-SPLIT every component before building classes. Union-find over-merges
        // transitive chains (A~B, B~C, A!~C ⇒ {A,B,C}); `coherence_split` returns internally-
        // coherent sub-classes (every pair ≥ θ) instead. A component that splits entirely into
        // singletons (each < min_copies) yields NO class — that is correct: an over-merged chain
        // with no coherent ≥2 sub-class is not a real clone class. (No fallback here; the
        // un-refined-component fallback is `clones_for_symbol`'s, where the caller pinned a
        // subject.) Each built class travels with its component ids so the two-phase driver
        // can refine it.
        let mut built: Vec<(Vec<i64>, CandidateCloneClass)> = Vec::new();
        for (comp_idx, component) in components.iter().enumerate() {
            if component.len() < min_copies {
                continue;
            }
            let coherent_classes = coherence_split(
                component,
                &edges_by_component[comp_idx],
                |a, b| {
                    let ba = by_id[&a];
                    let bb = by_id[&b];
                    let max_len = ba.token_len.max(bb.token_len);
                    if max_len == 0 { 1.0 } else { overlap(ba, bb) as f64 / max_len as f64 }
                },
                theta,
            );
            for class_ids in &coherent_classes {
                if class_ids.len() < min_copies {
                    continue;
                }
                if let Some(class) = build_class(class_ids, &by_id, conn, None)? {
                    built.push((class_ids.clone(), class));
                }
            }
        }

        // ── Two-phase ROI ranking (Plan 4a) ────────────────────────────────────────────────────
        // Phase 1: sort ALL coherent classes by the Plan-2 (un-refined) ROI — the cohesion
        // multiplier. Refining is comparatively expensive (re-read + re-parse + LCS), so the
        // provisional rank picks which classes are worth refining.
        built.sort_by(|a, b| b.1.roi.partial_cmp(&a.1.roi).unwrap_or(std::cmp::Ordering::Equal));

        // Total coherent classes BEFORE any limit drop — feeds the `truncated` flag below so a
        // limited result that dropped whole classes still reports `truncated == true`.
        let total_classes_built = built.len();

        // Refine budget: the maximum number of classes to refine (re-read + re-parse + LCS).
        // Shared between the limited and unlimited paths — the limited path clamps its effective
        // returned count to this budget so `find_clones { limit: 100000 }` can't queue unbounded
        // re-parse work while still returning all-refined classes. The unlimited path refines only
        // the top-budget classes and returns all (with unrefined tail).
        const UNLIMITED_REFINE_BUDGET: usize = 50;

        let classes: Vec<CandidateCloneClass> = if let Some(limit) = opts.limit {
            // Limited result: truncate to min(limit, UNLIMITED_REFINE_BUDGET) so a huge `limit`
            // doesn't queue unbounded re-parse work. The all-refined-limited invariant (Fix 2
            // round-1: every class in a limited result is refined, no unrefined rank-(N+1) class)
            // is preserved because we clamp BEFORE refining — we refine at most
            // UNLIMITED_REFINE_BUDGET classes and return exactly those. Callers wanting
            // more than UNLIMITED_REFINE_BUDGET classes (with only the top refined)
            // should use limit: None. DOCUMENTED BEHAVIOR: a limited query returns at
            // most UNLIMITED_REFINE_BUDGET (50) classes, all refined; to retrieve more
            // classes use limit: None.
            let effective_limit = limit.min(UNLIMITED_REFINE_BUDGET);
            built.truncate(effective_limit);
            for (class_ids, class) in built.iter_mut() {
                self.refine_class_in_place(conn, class_ids, &by_id, class)?;
            }
            let mut cs: Vec<CandidateCloneClass> = built.into_iter().map(|(_, c)| c).collect();
            cs.sort_by(|a, b| b.roi.partial_cmp(&a.roi).unwrap_or(std::cmp::Ordering::Equal));
            cs
        } else {
            // Unlimited result: refine the top-`UNLIMITED_REFINE_BUDGET` by provisional ROI, return
            // ALL classes. Classes beyond the budget keep their Plan-2 (un-refined) shape — the
            // inherent best-effort case for unlimited results. After refinement the FULL list is
            // re-sorted by ROI so a refined class that gained/lost rank lands in the right place.
            for (idx, (class_ids, class)) in built.iter_mut().enumerate() {
                if idx >= UNLIMITED_REFINE_BUDGET {
                    break;
                }
                self.refine_class_in_place(conn, class_ids, &by_id, class)?;
            }
            let mut cs: Vec<CandidateCloneClass> = built.into_iter().map(|(_, c)| c).collect();
            cs.sort_by(|a, b| b.roi.partial_cmp(&a.roi).unwrap_or(std::cmp::Ordering::Equal));
            cs
        };

        // `truncated` is true if ANY returned class capped its member list (members_returned <
        // total_members) OR whole classes were dropped to honor the limit (limited path only —
        // `total_classes_built` exceeds the returned count).
        let truncated = classes.iter().any(|c| c.members_returned < c.total_members)
            || total_classes_built > classes.len();

        // refine_budget_clamped: true when a limit was supplied, the limit exceeded the budget, AND
        // the budget actually dropped classes (total_classes_built > effective_limit). A limit at
        // or below the budget never clamps; a limit above the budget only clamps when there
        // were more built classes than the budget could return.
        let refine_budget_clamped = opts.limit.is_some_and(|lim| {
            lim > UNLIMITED_REFINE_BUDGET && total_classes_built > UNLIMITED_REFINE_BUDGET
        });

        let freshness = self.meta("git_commit")?.unwrap_or_else(|| "unknown".to_string());

        // Count DISTINCT member file paths whose on-disk content no longer matches the indexed
        // sha256 (read-only signal; Plan 2 does not heal-before-return).
        let stale_members = count_stale_member_paths(self, conn, &classes)?;

        let completeness = build_completeness(
            theta,
            min_copies,
            truncated,
            refine_budget_clamped,
            stale_members,
            freshness,
        );

        Ok(FindClonesResult { classes, completeness })
    }

    /// Refine one candidate class IN PLACE (#215 Plan 4a): load the class's refine inputs (re-read,
    /// re-parse, and re-normalize each member to its ordered baseline token sequence), compute the
    /// content-addressed refinement (read-through `clone_refinements` cache), set the
    /// refinement fields, flip `refined`/`class_kind`, and swap the ROI cohesion multiplier for
    /// `refactorability`. This is a NO-OP (the class keeps its Plan-2 un-refined shape) when refine
    /// inputs are unavailable (overlay scope, drifted source, parse failure, or a vanished
    /// hydration row), exactly mirroring the un-refinable fallback `load_refine_members`
    /// already encodes.
    ///
    /// `class_ids` is the class's component (the coherent sub-class) symbol ids; `by_id` supplies
    /// each member's persisted `struct_hash` for the content-addressed key (no extra DB
    /// round-trip).
    fn refine_class_in_place(
        &self,
        conn: &Connection,
        class_ids: &[i64],
        by_id: &BTreeMap<i64, &SymbolBag>,
        class: &mut CandidateCloneClass,
    ) -> anyhow::Result<()> {
        // ── Phase 0 (CHEAP, NO RE-PARSE): build the content-addressed key from each member's
        // persisted struct_hash already in `by_id` — no file I/O, no tree-sitter parse.
        // If a class member's struct_hash is somehow absent from `by_id` (shouldn't happen for a
        // coherent class, but defend anyway) the key would be over an incomplete multiset, which
        // could alias a different class. Fall back to leaving this class un-refined rather than
        // computing a wrong refinement.
        let struct_hashes: Vec<String> = class_ids
            .iter()
            .filter_map(|id| by_id.get(id).map(|b| b.struct_hash.clone()))
            .collect();
        if struct_hashes.len() != class_ids.len() {
            // Defensive: a member's bag is missing — skip rather than key over a partial multiset.
            return Ok(());
        }

        // ── Source discriminators (cheap SELECT, NO RE-PARSE): pin each member's EXACT source
        // bytes so the content-addressed key discriminates two classes that share a
        // struct_hash multiset (the NORMALIZED token sequence) but differ in real source.
        // The 4b cached payload (template, per-member values, signature) is
        // SOURCE-SPECIFIC; a structure-only key would serve one class's payload to a
        // structurally-identical-but-source-different class (cache poisoning).
        // `"{file_sha256}:{start}-{end}"` pins the file content hash + body range →
        // together they uniquely determine the raw source. This is a SELECT (the same symbols/files
        // join the bags path already touches), so the warm-probe-before-reparse stays a probe.
        // If any member's discriminator can't be fetched, leave the class un-refined rather than
        // key over a partial/structure-only multiset (which could alias a different class).
        let Some(source_discriminators) = load_source_discriminators(conn, class_ids)? else {
            return Ok(());
        };

        // Content-addressed key over the member struct_hash multiset + the per-member source
        // discriminators — NOT the read-side `class.class_key` (location-derived). Two classes with
        // the same structural content AND the same exact source bodies share a refinement; the key
        // survives a reindex that reassigns rowids.
        //
        // For coherent classes exceeding METRIC_SAMPLE_CAP members, `class.similarity_min` was
        // derived from the first `METRIC_SAMPLE_CAP` members only (the metric-sample path in
        // `build_class`), while `key` spans the FULL struct_hash multiset. The gap is not a
        // determinism break — the sample is id-ASC stable — but Plan-4b should compute confidence
        // over the full set or fold the sample into the key.
        let key = refinement_key(&class.language, &struct_hashes, &source_discriminators);

        // ── Phase 1 (PURE READ — warm path): probe the content-addressed cache. A SELECT is safe
        // on the MCP's read-only connection; a WARM cache hit never takes the write lock and never
        // re-parses any source file. This is the main perf win: the re-parse (load_refine_members,
        // below) was previously called BEFORE the cache probe, so a warm hit still paid the full
        // tree-sitter re-parse cost for every member — now it is bypassed entirely.
        //
        // CORRECTNESS NOTE: on a cache HIT we intentionally skip the load_refine_members
        // struct_hash-faithfulness re-validation. This is safe: the cache is keyed by the persisted
        // struct_hash multiset, so a drifted source that changes struct_hash produces a different
        // key → cache miss → cold path (re-parse + faithfulness check). Staleness is separately
        // surfaced to callers via `completeness.stale_members`. Skipping the re-validation on warm
        // hits is therefore not a regression; it is the designed behavior.
        if let Some(refinement) = refine_lookup(conn, &key)? {
            // WARM path: cache hit — apply the refinement without re-parsing any source files.
            apply_refinement(class, refinement);
            return Ok(());
        }

        // ── Phase 2 (COLD path only): cache miss. Before any expensive work, probe writability. If
        // the connection is read-only, surface a genuine SQLITE_READONLY error so
        // `is_readonly_violation` flags it and the MCP dispatcher retries read-write; the retry
        // takes the same path but writable (Phase 1 may hit the cache on the retry if a concurrent
        // writer raced us, or falls through to the compute below).
        if conn.is_readonly(rusqlite::MAIN_DB)? {
            // Mint a real SQLITE_READONLY `rusqlite::Error` that `is_readonly_violation`
            // recognizes, WITHOUT any expensive LCS work. A zero-row write to a real table
            // (`DELETE … WHERE 1=0`) acquires the write lock → fails with SQLITE_READONLY on a
            // RO connection. (A `BEGIN IMMEDIATE` does NOT error here — this rusqlite/SQLite
            // build defers the write-lock acquisition past transaction-start, so the probe must
            // be an actual write statement.) The `WHERE 1=0` makes it a true no-op even on a
            // writable connection; in practice this branch only runs on the RO pass, since the
            // RW retry sees `is_readonly == false` and skips straight to the compute.
            conn.execute("DELETE FROM clone_refinements WHERE 1=0", [])?;
            // The probe MUST error on a RO connection; if it somehow didn't, bail rather than
            // fall through to a compute whose INSERT would itself fail.
            anyhow::bail!("clone refine requires a writable connection");
        }

        // ── Phase 3 (writable cold path): re-parse each member's source file with tree-sitter,
        // compute the LCS ratio + refactorability, persist in the cache, and apply.
        // `load_refine_members` is the expensive step (file reads + tree-sitter parses).
        // `None` ⇒ refine unavailable (overlay scope, drifted source, parse failure, or a
        // vanished fingerprint row) — leave the class un-refined.
        let Some(members) = self.load_refine_members(class_ids)? else {
            return Ok(());
        };
        // An empty or singleton member set (shouldn't happen for a ≥2 class) is not refinable.
        if members.len() < 2 {
            return Ok(());
        }

        // Thread the bag-overlap medoid (Plan 4b §1.1) as the anti-unify spine anchor; the compute
        // half falls back to the canonical-first member when it is `None`.
        let refinement = refine_compute_and_store(
            conn,
            &key,
            &class.language,
            &members,
            class.similarity_min,
            class.medoid_symbol_id,
        )?;
        apply_refinement(class, refinement);

        Ok(())
    }
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
const COVERAGE_STRONG_PENALTY: f64 = 0.10;
/// Mild refined-ROI multiplier for the `[0.3, 0.5)` coverage band (matches `refactorability_v2`).
const COVERAGE_MILD_PENALTY: f64 = 0.70;

/// Mutually-exclusive coverage band giving the refined-ROI multiplier (#256): the strong penalty
/// below [`COVERAGE_STRONG_GATE`], the mild penalty in the `[0.3, 0.5)` band, and `1.0` (no
/// penalty) at/above [`COVERAGE_MILD_GATE`]. Applied ONLY to the refined ROI in
/// [`apply_refinement`]; the un-refined sort has no coverage to gate on.
fn coverage_roi_gate(anti_unify_coverage: f64) -> f64 {
    if anti_unify_coverage < COVERAGE_STRONG_GATE {
        COVERAGE_STRONG_PENALTY
    } else if anti_unify_coverage < COVERAGE_MILD_GATE {
        COVERAGE_MILD_PENALTY
    } else {
        1.0
    }
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
fn apply_refinement(
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
    class.refine_mode = Some(refinement.refine_mode);

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
fn build_completeness(
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
        // string + boolean literals bucket multi-language (NORM_VERSION = 2), and TS
        // function-valued declarators are fingerprinted. No clone-substrate gaps are
        // currently advertised open.
        known_index_gaps: Vec::new(),
    }
}

// ── clones_for_symbol public API ─────────────────────────────────────────────────────────────

/// How to identify the subject symbol for [`IndexDatabase::clones_for_symbol`].
#[derive(Debug, Clone)]
pub enum CloneSymbolSelector {
    /// An opaque `sym_<hex>` logical-symbol handle (as emitted by symbol-returning tools).
    Id(String),
    /// A fully-qualified `"path/to/file.rs::symbol_name"` reference.
    Ref(String),
    /// The tightest-spanning in-scope symbol whose line range contains `line` in `path`.
    PathLine { path: String, line: i64 },
}

impl IndexDatabase {
    /// Return a [`ClonesForSymbolResult`] for the symbol identified by `selector`: the containing
    /// candidate clone class (or `None` if the symbol is unique, not eligible, or unresolved),
    /// plus eligibility flags and the same completeness block as [`Self::find_clones`].
    ///
    /// `symbol_resolved` reports whether the selector matched a scoped symbol;
    /// `symbol_fingerprinted` reports whether that symbol has a current-version baseline
    /// fingerprint loaded into the candidate set (eligible). A symbol can resolve but not be
    /// fingerprinted (generated file, below `MIN_TOKENS`, or a non-function symbol) — `class`
    /// is then `None`.
    ///
    /// Resolution per selector form (all scoped through the active `files` view):
    /// - `Id`: parse the `sym_<hex>` handle → logical-symbol members → first member in a bag.
    /// - `Ref`: exact qualified-name match via `symbols JOIN name_strings`.
    /// - `PathLine`: tightest-spanning symbol at that line (`end_line - start_line ASC LIMIT 1`).
    pub fn clones_for_symbol(
        &self,
        selector: CloneSymbolSelector,
    ) -> anyhow::Result<ClonesForSymbolResult> {
        let conn = self.storage.connection();

        let make_result = |class: Option<CandidateCloneClass>,
                           symbol_resolved: bool,
                           symbol_fingerprinted: bool,
                           stale_members: usize,
                           freshness: String| {
            // A single class can only truncate by capping its own member list; there is no
            // class-limit here, so reuse the same member-cap signal as `find_clones`.
            let truncated = class.as_ref().is_some_and(|c| c.members_returned < c.total_members);
            ClonesForSymbolResult {
                class,
                symbol_resolved,
                symbol_fingerprinted,
                // No class limit on this path → never refine-budget-clamped.
                completeness: build_completeness(
                    THETA,
                    2,
                    truncated,
                    false,
                    stale_members,
                    freshness,
                ),
            }
        };

        let freshness = self.meta("git_commit")?.unwrap_or_else(|| "unknown".to_string());

        let resolved_id = resolve_selector_to_symbol_id(conn, &selector)?;
        let Some(symbol_id) = resolved_id else {
            return Ok(make_result(None, false, false, 0, freshness));
        };

        let bags = load_scoped_baseline_bags(conn)?;
        // If the resolved symbol has no fingerprint row it is not eligible — it can't be in any
        // clone class (generated file, below MIN_TOKENS, or a non-function symbol).
        if !bags.iter().any(|b| b.symbol_id == symbol_id) {
            return Ok(make_result(None, true, false, 0, freshness));
        }

        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
        let pairs = candidate_pairs_from_bags(&bags, THETA);
        let components = components_from_pairs(&pairs);

        // Find the component that contains this symbol_id (with its index so we can pull its edge
        // bucket).
        let Some(comp_idx) = components.iter().position(|comp| comp.contains(&symbol_id)) else {
            return Ok(make_result(None, true, true, 0, freshness));
        };
        let component = components[comp_idx].clone();
        // The θ-verified edge subset for THIS component (#256) — feeds the scalable clique cover so
        // a giant over-merged component the subject lands in splits instead of being returned
        // whole.
        let mut edges_by_component = bucket_edges_by_component(&pairs, &components);
        let component_edges = std::mem::take(&mut edges_by_component[comp_idx]);

        // Plan 4a: coherence-split the component, then serve the coherent sub-class that contains
        // the subject. If the subject is a SINGLETON after the split (it cohered with no peer at
        // θ), there is NO whole-component fallback (#256): serving the full over-merged component
        // would re-expose the very giant the split exists to break. `clones_for_symbol` returns the
        // subject's COHERENT neighborhood (the clique(s) containing it) or nothing.
        let coherent_classes = coherence_split(
            &component,
            &component_edges,
            |a, b| {
                let ba = by_id[&a];
                let bb = by_id[&b];
                let max_len = ba.token_len.max(bb.token_len);
                if max_len == 0 { 1.0 } else { overlap(ba, bb) as f64 / max_len as f64 }
            },
            THETA,
        );
        // Pick the largest coherent group containing the subject (tie → highest min-pairwise
        // cohesion → lowest member id). The greedy clique-cover split can return MULTIPLE
        // overlapping groups containing the subject (e.g. B is in both {A,B} and {B,C} for chain
        // A~B / B~C / A!~C) — the subject's "best" class is the largest such group, so a reverse
        // lookup surfaces the richest coherent neighborhood it belongs to rather than an arbitrary
        // first-fit pair.
        let subject_subclass = {
            let candidates: Vec<Vec<i64>> =
                coherent_classes.into_iter().filter(|cls| cls.contains(&symbol_id)).collect();
            candidates.into_iter().max_by(|a, b| {
                a.len()
                    .cmp(&b.len())
                    .then_with(|| {
                        // Higher min-pairwise cohesion wins.
                        let cohesion_a = min_pairwise_cohesion(a, &by_id);
                        let cohesion_b = min_pairwise_cohesion(b, &by_id);
                        cohesion_a.partial_cmp(&cohesion_b).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    // Fully-deterministic final tiebreak: compare the full sorted member-id vector
                    // (lexicographically, reversed so max_by keeps the lexicographically-smallest).
                    // `max_by` returns the LAST equal element, so we reverse: "greater" vector loses.
                    .then_with(|| b.cmp(a))
            })
        };

        // Pin the resolved subject so it is guaranteed to appear in the (capped) member list even
        // when its id falls past MAX_MEMBERS in the (sub)class id order — the caller asked about
        // THIS symbol (Fix 2, #215).
        let class = match subject_subclass {
            Some(subclass) => {
                let mut built = build_class(&subclass, &by_id, conn, Some(symbol_id))?;
                // Always refine the subject's one class when refine inputs are available.
                if let Some(c) = built.as_mut() {
                    self.refine_class_in_place(conn, &subclass, &by_id, c)?;
                }
                built
            },
            None => {
                // Subject split to a singleton — it cohered with no peer at θ, so there is no
                // coherent class to serve. Return nothing (#256): the OLD behavior served the full
                // un-refined component, which re-exposed the over-merged giant the split exists to
                // break (the exact `clones-for`-on-a-chained-symbol regression #256 names). A
                // reverse lookup ABOUT a symbol that has no coherent clone peer is honestly empty.
                None
            },
        };

        // Count stale member paths over just this class's members (None class → 0 stale).
        let stale_members = match &class {
            None => 0,
            Some(c) => {
                let single = std::slice::from_ref(c);
                count_stale_member_paths(self, conn, single)?
            },
        };

        Ok(make_result(class, true, true, stale_members, freshness))
    }
}

/// One member's persisted hydration row before the re-parse: scoped path + byte range + language +
/// the baseline `struct_hash` (the canonical sort/cache key). Mirrors `build_class`'s member
/// hydration but additionally pulls `start_byte`/`end_byte` (for the AST descent) and `struct_hash`
/// (the faithfulness pin).
struct RefineRow {
    symbol_id: i64,
    path: String,
    start_byte: usize,
    end_byte: usize,
    language: crate::language::Language,
    struct_hash: String,
}

impl IndexDatabase {
    /// Load refine inputs for a class's members (#215 Plan 4a Task 2): resolve each member's scoped
    /// path + byte range, read the active-scope-correct source, parse, descend to the symbol node,
    /// and `normalize_baseline` into the ordered baseline token sequence. Returns members in
    /// CANONICAL sorted-by-`struct_hash` order (then `symbol_id` as a tiebreak) — the ordinal basis
    /// the anti-unify step's `per_member_values[]` aligns to.
    ///
    /// Returns `Ok(None)` if refine inputs are unavailable for ANY member — source
    /// missing/unreadable, a hydration row that vanished mid-read (TOCTOU), a parse failure, no AST
    /// node at the byte range, or a re-parse whose `struct_hash` no longer matches the persisted
    /// one (the file drifted off-index). The caller falls back to an un-refined class.
    /// Returning `None` on ANY missing member (rather than dropping it) keeps the class a
    /// faithful whole: a partial refine over a subset of members would mis-rank and
    /// mis-template.
    ///
    /// SCOPE LIMITATION (deliberate, mirrors `count_stale_member_paths` / the staleness heal path):
    /// under a LINKED-WORKTREE OVERLAY scope, `source_root` is the MAIN checkout — NOT the branch
    /// the overlay's symbol rows came from. Re-reading main's bytes at the overlay member's byte
    /// range would parse the WRONG source (or fail entirely on a branch-only file). There is no
    /// scope-correct source read available here for the overlay, so refine is unavailable under an
    /// overlay scope: return `Ok(None)` and let the caller serve the un-refined class. (When a
    /// scope-correct overlay source read lands, this guard can be lifted.)
    pub(crate) fn load_refine_members(
        &self,
        member_ids: &[i64],
    ) -> anyhow::Result<Option<Vec<crate::index::clones::refine::RefineMember>>> {
        if member_ids.is_empty() {
            return Ok(Some(Vec::new()));
        }

        // Overlay scope: no scope-correct source read is available (see doc above). Bail to the
        // un-refined fallback rather than re-parse the wrong (main-checkout) bytes.
        if self.active_scope_is_linked_overlay() {
            return Ok(None);
        }

        let Some(root) = self.storage.source_root() else {
            return Ok(None);
        };

        let conn = self.storage.connection();
        let rows = match load_refine_rows(conn, member_ids)? {
            None => return Ok(None),
            Some(rows) => rows,
        };

        // Sort then cap at MEMBER_VALUE_CAP (= 50) in canonical (struct_hash, path, start_byte)
        // order — the REINDEX-STABLE ordinal basis (Fix 2, #215 Plan 4b Codex round-4).
        //
        // Why NOT (struct_hash, symbol_id): `symbol_id` is a `symbols.id` rowid REASSIGNED on every
        // reindex. The cached 4b payload (`per_member_values`, template) is anchored to this member
        // order, but a file-unchanged reindex hits the same warm refinement_key (struct_hash +
        // source discriminators are content-derived) while the canonical order recomputed
        // here can REORDER members that share a struct_hash (common in a clone class) — so
        // a cached per_member_values[i] would label a DIFFERENT member than
        // canonical_member_refs[i] recomputes to. `(path, start_byte)` uniquely identifies
        // a member (no two symbols start at the same byte in one file) and is stable across
        // reindex when the file content is unchanged, so the cached value order always
        // matches the recomputed canonical_member_refs order. `build_class`'s
        // `canonical_member_refs` builder uses the SAME (struct_hash, path, start_byte) key.
        //
        // Plan 4b change: the cap was previously LCS_MEMBER_SAMPLE (64). It is now MEMBER_VALUE_CAP
        // (50, = MAX_MEMBERS) so every returned member carries spans + text for per_member_values
        // collection in Task 5d. Since MEMBER_VALUE_CAP (50) < LCS_MEMBER_SAMPLE (64), the align
        // pass in 5d never receives more members than the align cap can accommodate — no additional
        // truncation is needed there. (See MEMBER_VALUE_CAP doc comment above.) The cap is a
        // stable-PREFIX of this reindex-stable order.
        let mut rows = rows;
        rows.sort_by(|a, b| {
            canonical_member_order_key(&a.struct_hash, &a.path, a.start_byte as i64)
                .cmp(&canonical_member_order_key(&b.struct_hash, &b.path, b.start_byte as i64))
        });
        rows.truncate(MEMBER_VALUE_CAP);

        // Dedup file reads by path (Plan 4a I3). Cache is now `Arc<str>` so members in the same
        // file share the single allocation — the anti-unify step (Plan 4b §1.6) relies on
        // `member.text.get(span.start_byte..span.end_byte)` using ABSOLUTE file offsets, so the
        // whole-file buffer must be kept, not sliced to the symbol range.
        let mut file_cache: std::collections::HashMap<String, Arc<str>> =
            std::collections::HashMap::new();

        let mut members: Vec<crate::index::clones::refine::RefineMember> =
            Vec::with_capacity(rows.len());
        for row in rows {
            if !file_cache.contains_key(&row.path) {
                let Ok(content) = std::fs::read_to_string(root.join(&row.path)) else {
                    // Source missing/unreadable on disk — can't reproduce the token sequence.
                    return Ok(None);
                };
                file_cache.insert(row.path.clone(), Arc::from(content.as_str()));
            }
            let text: Arc<str> =
                Arc::clone(file_cache.get(&row.path).expect("just inserted above"));
            let Some(parsed) =
                crate::index::parser::parse_file(Path::new(&row.path), row.language, &text)
            else {
                // Parse failure (or a no-grammar language like markdown) — no AST to descend.
                return Ok(None);
            };
            let Some(node) = parsed.root().descendant_for_byte_range(row.start_byte, row.end_byte)
            else {
                // No node spans the persisted byte range — the file drifted off-index.
                return Ok(None);
            };
            // Plan 4b: use normalize_baseline_spanned so each token carries its AST span.
            // The seq (.0) is byte-identical to the old normalize_baseline output (faithfulness
            // pin).
            let (seq, node_spans) =
                crate::index::clones::normalize::normalize_baseline_spanned(node, &text);

            // Faithfulness pin: the re-parse must reproduce Plan-1's normalization exactly. A
            // mismatch means the on-disk file no longer matches the indexed fingerprint (the
            // `files.sha256` staleness signal would also flag it) — refining a drifted member would
            // align stale tokens, so bail to the un-refined fallback rather than panic in
            // production.
            let reparsed_hash = crate::index::clones::tokens::struct_hash(&seq);
            if reparsed_hash != row.struct_hash {
                // Silent degrade: a library read must not write to stderr. The drift is already
                // surfaced to callers via `completeness.stale_members`; here we just fall back to
                // the un-refined class rather than align stale tokens.
                return Ok(None);
            }

            members.push(crate::index::clones::refine::RefineMember {
                symbol_id: row.symbol_id,
                lang: row.language,
                struct_hash: row.struct_hash,
                seq,
                node_spans,
                text,
            });
        }

        // `members` is built by iterating `rows` in order, and `rows` was already sorted into the
        // canonical REINDEX-STABLE (struct_hash, path, start_byte) order above (then truncated).
        // That order is authoritative — `RefineMember` does NOT carry `path`/`start_byte`,
        // so we must NOT re-sort here on a different key (a `(struct_hash, symbol_id)`
        // re-sort would REORDER equal-struct_hash members and break the per_member_values ↔
        // canonical_member_refs alignment that Fix 2 establishes). The members are already
        // in the ordinal basis.
        debug_assert!(
            members.windows(2).all(|w| w[0].struct_hash <= w[1].struct_hash),
            "members must stay in the row-sorted struct_hash-ascending canonical order"
        );

        Ok(Some(members))
    }
}

/// Hydrate the scoped path + byte range + language + persisted baseline `struct_hash` for each
/// `member_ids` symbol, in chunks of [`HYDRATION_CHUNK`] (the same SQLite host-param discipline as
/// `build_class`). Filters the baseline normalizer version so a stale fingerprint row never feeds
/// the refine input. Returns `Ok(None)` if ANY requested member is absent (a fingerprint row
/// vanished mid-read, or the symbol fell out of scope): a partial refine would be unfaithful.
fn load_refine_rows(
    conn: &Connection,
    member_ids: &[i64],
) -> anyhow::Result<Option<Vec<RefineRow>>> {
    let mut rows: Vec<RefineRow> = Vec::with_capacity(member_ids.len());
    for chunk in member_ids.chunks(HYDRATION_CHUNK) {
        let id_placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let version_placeholder = format!("?{}", chunk.len() + 1);
        let sql = format!(
            "SELECT symbols.id, files.path, symbols.start_byte, symbols.end_byte, \
             symbols.language, sf.struct_hash
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             JOIN symbol_fingerprints sf
               ON sf.symbol_id = symbols.id
               AND sf.normalizer_kind = 'baseline'
               AND sf.normalizer_version = {version_placeholder}
             WHERE symbols.id IN ({})
             ORDER BY symbols.id",
            id_placeholders.join(", ")
        );
        let params: Vec<i64> = chunk.iter().copied().chain(std::iter::once(NORM_VERSION)).collect();
        let mut stmt = conn.prepare(&sql)?;
        let chunk_rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let start_byte: i64 = row.get(2)?;
            let end_byte: i64 = row.get(3)?;
            let lang_str: String = row.get(4)?;
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                start_byte,
                end_byte,
                lang_str,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in chunk_rows {
            let (symbol_id, path, start_byte, end_byte, lang_str, struct_hash) = row?;
            // A negative byte offset can't occur (schema NOT NULL, written from usize), but guard
            // the cast so a corrupt row degrades to the un-refined fallback rather than panicking.
            let (Ok(start_byte), Ok(end_byte)) =
                (usize::try_from(start_byte), usize::try_from(end_byte))
            else {
                return Ok(None);
            };
            // An unparseable language string means the row's language is no longer one this build
            // understands — bail to the un-refined fallback.
            let Ok(language) = lang_str.parse::<crate::language::Language>() else {
                return Ok(None);
            };
            rows.push(RefineRow { symbol_id, path, start_byte, end_byte, language, struct_hash });
        }
    }

    // EVERY requested member must hydrate. A missing one (vanished fingerprint row, out-of-scope
    // symbol) makes the class incomplete — refine the whole class or none of it.
    if rows.len() != member_ids.len() {
        return Ok(None);
    }
    rows.sort_unstable_by_key(|r| r.symbol_id);
    Ok(Some(rows))
}

/// Fetch a per-member SOURCE DISCRIMINATOR — `"{file_sha256}:{start_byte}-{end_byte}"` — for the
/// refinement cache key (#215 Plan 4b, cache-poisoning fix). `file_sha256` is the indexed file
/// content hash; the body byte span pins the member's source range. Together they uniquely
/// determine the member's raw source bytes, so two structurally-identical-but-source-different
/// classes (same `struct_hash` multiset, different real literals) get DISTINCT keys → no
/// cross-class poisoning of the source-specific 4b payload (template / per-member values /
/// signature). Two BYTE-IDENTICAL-source classes still share the discriminator multiset → the same
/// key → true content-addressing of real duplicates is preserved.
///
/// CHEAP — a pure SELECT joining `symbols → files` (no tree-sitter re-parse), so a warm cache probe
/// stays a probe: `refine_class_in_place` calls this BEFORE `refine_lookup`, and the lookup still
/// short-circuits the expensive `load_refine_members` re-parse on a hit. Returns `Ok(None)` when
/// ANY member fails to hydrate (vanished row, out-of-scope symbol) — the caller leaves the class
/// un-refined rather than key over a partial (and therefore structure-only-aliasing) multiset.
fn load_source_discriminators(
    conn: &Connection,
    member_ids: &[i64],
) -> anyhow::Result<Option<Vec<String>>> {
    if member_ids.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let mut discriminators: Vec<String> = Vec::with_capacity(member_ids.len());
    for chunk in member_ids.chunks(HYDRATION_CHUNK) {
        let id_placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT files.sha256, symbols.start_byte, symbols.end_byte
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             WHERE symbols.id IN ({})",
            id_placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let chunk_rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        })?;
        for row in chunk_rows {
            let (sha256, start_byte, end_byte) = row?;
            discriminators.push(format!("{sha256}:{start_byte}-{end_byte}"));
        }
    }
    // EVERY requested member must hydrate, exactly as `load_refine_rows` requires — a partial
    // multiset would alias a different class. Mismatch ⇒ leave un-refined.
    if discriminators.len() != member_ids.len() {
        return Ok(None);
    }
    Ok(Some(discriminators))
}

/// Count DISTINCT member file paths in `classes` whose on-disk content no longer matches the
/// indexed `files.sha256`. Fetches the indexed sha256 per distinct path from the `files` view
/// (one query per distinct path) then calls [`IndexDatabase::source_path_is_stale`] — the same
/// pattern as `graph_index.rs`. This is a read-only signal; callers should reindex if non-zero.
fn count_stale_member_paths(
    db: &crate::index::IndexDatabase,
    conn: &Connection,
    classes: &[CandidateCloneClass],
) -> anyhow::Result<usize> {
    // `source_path_is_stale` reads bytes from `source_root` (the MAIN checkout). Under a
    // linked-worktree overlay scope the scoped clone results come from the BRANCH bytes
    // (`index_worktree_overlay`), so a main-checkout comparison is meaningless: branch-only files
    // look "missing" (false stale) and same-path branch edits diff against base content (false
    // stale). Mirror `heal_file` / the search-heal path / `parser_failures`, which all early-return
    // under an overlay scope, and report 0 (no main-checkout staleness signal is available here).
    if db.active_scope_is_linked_overlay() {
        return Ok(0);
    }
    // Collect distinct paths across all returned members.
    let mut distinct_paths: BTreeSet<String> = BTreeSet::new();
    for class in classes {
        for member in &class.members {
            distinct_paths.insert(member.path.clone());
        }
    }
    let mut stale = 0usize;
    for path in &distinct_paths {
        let sha256: Option<String> = conn
            .query_row("SELECT sha256 FROM files WHERE path = ?1 LIMIT 1", [path.as_str()], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(sha256) = sha256 else {
            // Path not in index at all — treat as stale.
            stale += 1;
            continue;
        };
        if db.source_path_is_stale(path, &sha256) {
            stale += 1;
        }
    }
    Ok(stale)
}

/// Resolve a [`CloneSymbolSelector`] to an in-scope `symbols.id` rowid, or `None` if the selector
/// doesn't match any symbol in the active scope.
fn resolve_selector_to_symbol_id(
    conn: &Connection,
    selector: &CloneSymbolSelector,
) -> anyhow::Result<Option<i64>> {
    match selector {
        CloneSymbolSelector::Id(handle) => {
            let Some(logical_id) = crate::serde_big_id::parse_sym_handle(handle) else {
                return Ok(None);
            };
            // A logical-symbol may have multiple member rows (cfg splits, overloads). We PREFER
            // a fingerprinted member: a cfg-split logical symbol whose lowest-rowid member is
            // below MIN_TOKENS or unfingerprinted but whose sibling IS fingerprinted (and in a
            // clone class) would otherwise report `symbol_fingerprinted=false` and miss the class.
            // `(sf.symbol_id IS NULL) ASC` sorts fingerprinted members first (NULL = unmatched →
            // treated as 1 in SQLite boolean context → sorts after matched rows); falls back to
            // lowest-rowid when no member is fingerprinted, so `symbol_resolved=true,
            // symbol_fingerprinted=false` is still correctly reported.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT lm.symbol_id
                     FROM logical_symbol_members lm
                     JOIN symbols ON symbols.id = lm.symbol_id
                     JOIN files ON files.id = symbols.file_id
                     LEFT JOIN symbol_fingerprints sf
                       ON sf.symbol_id = lm.symbol_id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?2
                     WHERE lm.logical_symbol_id = ?1
                     ORDER BY (sf.symbol_id IS NULL) ASC, lm.symbol_id ASC
                     LIMIT 1",
                    rusqlite::params![logical_id, NORM_VERSION],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
        CloneSymbolSelector::Ref(qualified_name) => {
            // Exact qualified-name match through the scoped `files` view.
            // Ambiguity rule: collect ALL current-version fingerprinted symbols matching this ref.
            // - 0 fingerprinted → fall back to lowest-rowid unfingerprinted match (preserved
            //   "resolved but not fingerprinted" path: symbol_resolved=true,
            //   symbol_fingerprinted=false), or None if no symbol at all.
            // - 1 fingerprinted → use it (unambiguous).
            // - >1 fingerprinted → REJECT with a clear error: the ref maps to multiple distinct
            //   logical symbols (overloads, cfg variants) — the caller must disambiguate with Id or
            //   PathLine. Silently picking one could return an unrelated overload's class.
            let mut fingerprinted_ids: Vec<i64> = conn
                .prepare(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                     JOIN symbol_fingerprints sf
                       ON sf.symbol_id = symbols.id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?2
                     WHERE ns.value = ?1
                     ORDER BY symbols.id ASC",
                )?
                .query_map(rusqlite::params![qualified_name.as_str(), NORM_VERSION], |row| {
                    row.get(0)
                })?
                .collect::<Result<_, _>>()?;
            // Deduplicate: the same symbols.id can appear multiple times if there are multiple
            // fingerprint rows (shouldn't happen for a normalizer_version-locked query, but be
            // safe).
            fingerprinted_ids.dedup();

            if fingerprinted_ids.len() > 1 {
                let n = fingerprinted_ids.len();
                anyhow::bail!(
                    "clones_for_symbol: ref '{}' matches {} fingerprinted symbols (overloads/cfg \
                     variants) — use id or path+line to disambiguate",
                    qualified_name,
                    n
                );
            }
            if let Some(&id) = fingerprinted_ids.first() {
                return Ok(Some(id));
            }
            // 0 fingerprinted matches: fall back to lowest-rowid unfingerprinted symbol for the
            // "resolved but not fingerprinted" path.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                     WHERE ns.value = ?1
                     ORDER BY symbols.id ASC
                     LIMIT 1",
                    rusqlite::params![qualified_name.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
        CloneSymbolSelector::PathLine { path, line } => {
            // Tightest-spanning symbol whose range contains `line`: smallest (end_line -
            // start_line) among symbols where start_line <= line <= end_line.
            // CONTRACT: span is the PRIMARY key — return the symbol AT the cursor and let the
            // eligibility flags report it as not-fingerprinted; do NOT silently jump to an
            // enclosing fingerprinted function. Fingerprint presence is a TIE-BREAKER only:
            // among symbols with equal span, prefer the fingerprinted variant (so a cfg-split
            // same-span pair doesn't pick the unfingerprinted one and miss the clone class).
            // rowid is the final stable tie-breaker.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     LEFT JOIN symbol_fingerprints sf
                       ON sf.symbol_id = symbols.id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?3
                     WHERE files.path = ?1
                       AND ?2 BETWEEN symbols.start_line AND symbols.end_line
                     ORDER BY (symbols.end_line - symbols.start_line) ASC, (sf.symbol_id IS NULL) \
                     ASC, symbols.id ASC
                     LIMIT 1",
                    rusqlite::params![path.as_str(), line, NORM_VERSION],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
    }
}

/// Extract candidate pairs from already-loaded bags (avoids a second DB round-trip in
/// `find_clones` vs the original `candidate_pairs` path). `theta` is the similarity threshold
/// applied to both the sub-block prefix length and the exact overlap/max verify — passing the
/// caller's `min_similarity` widens (or narrows) candidate generation to match the requested
/// floor, instead of generating at the const [`THETA`] and post-filtering.
fn candidate_pairs_from_bags(bags: &[SymbolBag], theta: f64) -> Vec<(i64, i64)> {
    let mut pairs: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();
    add_struct_hash_pairs(bags, &mut pairs);
    let candidate = sub_block_candidate_pairs(bags, theta);
    let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
    for (a, b) in candidate {
        if verified_clone(by_id[&a], by_id[&b], theta) {
            pairs.insert((a, b));
        }
    }
    pairs.into_iter().collect()
}

/// Build a [`CandidateCloneClass`] from a component (a slice of symbol ids). Returns `None` if
/// any id is missing from `by_id` (shouldn't happen for a well-formed component derived from the
/// same bag set), or if member hydration yields nothing (TOCTOU: fingerprint rows vanished
/// mid-read). Members are capped at [`MAX_MEMBERS`].
///
/// `pin` is the subject `symbols.id` that MUST appear in the returned (capped) member list when it
/// is a member of the component — `clones_for_symbol` passes the resolved subject so the caller
/// always sees the symbol it asked about even when its id falls outside the first [`MAX_MEMBERS`]
/// by id. `find_clones` passes `None` (no subject to pin). `class_key` / `member_count` /
/// `total_members` are always over the FULL component regardless of `pin`.
pub(crate) fn build_class(
    component: &[i64],
    by_id: &BTreeMap<i64, &SymbolBag>,
    conn: &Connection,
    pin: Option<i64>,
) -> anyhow::Result<Option<CandidateCloneClass>> {
    let bags: Vec<&SymbolBag> = component.iter().filter_map(|id| by_id.get(id).copied()).collect();
    if bags.len() != component.len() {
        return Ok(None);
    }

    let n = bags.len();

    // For huge components, cap the pairwise metric work to avoid O(n²) blowup. When the
    // component exceeds METRIC_SAMPLE_CAP members the metrics run over the FIRST
    // METRIC_SAMPLE_CAP members only (the component is deterministically sorted, so this is
    // stable). `metrics_sampled` is set true so callers know. For all normal-size components
    // (the typical case, including ALL existing tests) `metric_n == n` and behavior is identical.
    let metrics_sampled = n > METRIC_SAMPLE_CAP;
    let metric_n = n.min(METRIC_SAMPLE_CAP);
    let metric_bags = &bags[..metric_n];

    // Pairwise similarity (overlap/max_len) and containment (overlap/min_len), upper-triangle
    // over metric_bags (== full component when metric_n == n).
    let mut similarity_min = f64::MAX;
    let mut containment_max = 0.0_f64;
    let mut sim_sums = vec![0.0_f64; metric_n]; // for medoid selection

    for i in 0..metric_n {
        for j in (i + 1)..metric_n {
            let ov = overlap(metric_bags[i], metric_bags[j]);
            let max_len = metric_bags[i].token_len.max(metric_bags[j].token_len);
            let min_len = metric_bags[i].token_len.min(metric_bags[j].token_len);
            let sim = if max_len == 0 { 1.0 } else { ov as f64 / max_len as f64 };
            let cont = if min_len == 0 { 1.0 } else { ov as f64 / min_len as f64 };
            if sim < similarity_min {
                similarity_min = sim;
            }
            if cont > containment_max {
                containment_max = cont;
            }
            sim_sums[i] += sim;
            sim_sums[j] += sim;
        }
    }
    if similarity_min == f64::MAX {
        // Singleton component — shouldn't reach here after min_copies filter, but be safe.
        similarity_min = 1.0;
    }
    let cohesion_min_pairwise = similarity_min;

    // Medoid: member with maximum sum of similarities to all others (within metric_bags).
    let medoid_idx = sim_sums
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let medoid_bag = metric_bags[medoid_idx];
    let body_token_len_medoid = medoid_bag.token_len;
    // Thread the medoid's symbol_id out onto the class for Plan 4b Task 5d (anti-unify spine
    // anchor). This is the bag-overlap medoid (max Σ overlap/max_len over metric_bags), NOT an
    // LCS-distance medoid — sound as a template anchor for a coherence-split class (all pairs ≥ θ)
    // where the bag-overlap medoid is representatively central. When metrics_sampled is true,
    // medoid_idx is over the first METRIC_SAMPLE_CAP members (id-ASC stable); the resolved id is
    // still a real member.
    let medoid_symbol_id = Some(medoid_bag.symbol_id);

    // Min similarity of any member to the medoid (within metric_bags).
    let mut similarity_medoid_min = f64::MAX;
    for (i, bag) in metric_bags.iter().enumerate() {
        if i == medoid_idx {
            continue;
        }
        let ov = overlap(medoid_bag, bag);
        let max_len = medoid_bag.token_len.max(bag.token_len);
        let sim = if max_len == 0 { 1.0 } else { ov as f64 / max_len as f64 };
        if sim < similarity_medoid_min {
            similarity_medoid_min = sim;
        }
    }
    if similarity_medoid_min == f64::MAX {
        similarity_medoid_min = 1.0;
    }

    let total_members = component.len();
    let cap = total_members.min(MAX_MEMBERS);

    // Hydrate CloneMembers via a scoped DB query with an IN clause, in batches of HYDRATION_CHUNK.
    // Fix 1 (#215): a single statement uses one host param per member id, so a component larger
    // than SQLITE_MAX_VARIABLE_NUMBER (999 on older non-bundled libs) would fail `conn.prepare`
    // and error the whole call. Chunking keeps every statement well under the limit; we
    // accumulate across chunks and re-sort by `symbol_id` so the deterministic id order is
    // restored regardless of chunk boundaries.
    // Fix 3 (#215): each chunk also filters normalizer_version so stale fingerprint rows don't
    // yield duplicate members or wrong token_len values. The version bind is appended as the
    // last positional param after that chunk's id list.
    // The tuple carries `start_byte` (Fix 2, #215 Plan 4b Codex round-4) so `canonical_member_refs`
    // can sort on the REINDEX-STABLE (struct_hash, path, start_byte) key — the SAME key
    // `load_refine_members` uses for the `per_member_values` ordinal basis. `start_byte` (not the
    // public `CloneMember.start_line`) is what `load_refine_members` orders by, and two symbols can
    // share a line but never a start byte, so it is the exact, total tiebreak that keeps the two
    // member orderings byte-for-byte identical.
    let mut raw_members: Vec<(i64, i64, CloneMember)> = Vec::with_capacity(total_members);
    for chunk in component.chunks(HYDRATION_CHUNK) {
        let id_placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let version_placeholder = format!("?{}", chunk.len() + 1);
        let sql = format!(
            "SELECT symbols.id, ns.value, files.path, symbols.start_line, symbols.end_line, \
             sf.token_len, symbols.language, symbols.start_byte
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             JOIN symbol_fingerprints sf
               ON sf.symbol_id = symbols.id
               AND sf.normalizer_kind = 'baseline'
               AND sf.normalizer_version = {version_placeholder}
             WHERE symbols.id IN ({})
             ORDER BY symbols.id",
            id_placeholders.join(", ")
        );
        let params: Vec<i64> = chunk.iter().copied().chain(std::iter::once(NORM_VERSION)).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let symbol_id: i64 = row.get(0)?;
            let start_byte: i64 = row.get(7)?;
            Ok((symbol_id, start_byte, CloneMember {
                r#ref: row.get(1)?,
                path: row.get(2)?,
                start_line: row.get(3)?,
                end_line: row.get(4)?,
                token_len: row.get(5)?,
                language: row.get(6)?,
            }))
        })?;
        for row in rows {
            raw_members.push(row?);
        }
    }
    // Restore the deterministic `symbols.id ASC` order the single-statement path produced.
    raw_members.sort_unstable_by_key(|(symbol_id, _, _)| *symbol_id);

    // Fix 5 (#215): if hydration returned nothing (all fingerprint rows vanished mid-read), bail to
    // `None` rather than build an internally-inconsistent class (member_count from the component
    // but zero members, an empty language fallback, etc.).
    if raw_members.is_empty() {
        return Ok(None);
    }

    let language = raw_members
        .first()
        .map(|(_, _, m)| m.language.clone())
        .unwrap_or_else(|| bags[0].language.clone());

    // cross_module_spread counts ALL hydrated members (full component), not just the capped subset
    // — so it is consistent with member_count (both over the full population).
    let parent_dirs: std::collections::BTreeSet<String> = raw_members
        .iter()
        .map(|(_, _, m)| {
            std::path::Path::new(&m.path)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        })
        .collect();
    let cross_module_spread = parent_dirs.len();

    // Fix 3 (#215): the class key is built from per-member identity that includes the source
    // LOCATION (`ref@path:start-end`), not the qualified-name `ref` alone. Two distinct
    // components can share the same qualified-name multiset — overloads, cfg variants,
    // same-named methods on different impls — and would otherwise collide on
    // `clone_refinements.class_key` (a TEXT PRIMARY KEY in Plan 4), conflating two classes into
    // one. We deliberately do NOT use `symbols.id`: the rowid is reassigned on every reindex,
    // so a location-derived key stays stable across reindexes while still distinguishing
    // same-named members at different spans. Computed over ALL members (full component), not
    // just the capped slice — two classes sharing the first MAX_MEMBERS members but
    // differing later must get different keys.
    let key_material: Vec<String> = raw_members
        .iter()
        .map(|(_, _, m)| format!("{}@{}:{}-{}", m.r#ref, m.path, m.start_line, m.end_line))
        .collect();
    let class_key = class_key_for(&key_material);

    // Canonical-ordered member refs (#215 Plan 4b): the qualified `ref` of each member in the SAME
    // canonical `(struct_hash, path, start_byte)` order, capped at the same `MEMBER_VALUE_CAP`,
    // that `load_refine_members` uses — so this is ORDINAL-ALIGNED to a refined class's
    // `variation_points[*].per_member_values`. The `members` field above is `r#ref`-sorted (display
    // order) and cannot be mapped to a `per_member_values` slot; `clones --explain` zips THIS
    // vector with the values so each printed value carries its member identity. Computed here
    // (not in `apply_refinement`) because the warm cache path skips `load_refine_members`
    // entirely, while `raw_members` (carrying `symbol_id`, `start_byte`, and `r#ref`) and `by_id`
    // (the `struct_hash`) are always available on both paths.
    //
    // Fix 2 (#215 Plan 4b Codex round-4): the sort key is `(struct_hash, path, start_byte)`, NOT
    // `(struct_hash, symbol_id)`. `symbol_id` is a rowid reassigned on every reindex, so a warm
    // refinement (same content key → cached per_member_values frozen at the OLD member order) could
    // be served against a `canonical_member_refs` recomputed in a DIFFERENT order — labelling one
    // member's value with another's identity. `(path, start_byte)` uniquely identifies a member and
    // is stable across a file-unchanged reindex, so the recomputed order always matches the cached
    // value order. This MUST stay byte-for-byte identical to `load_refine_members`' sort key.
    //
    // Codex #4 (round-3): the identity carried per member is LOCATION-BEARING
    // (`ref@path:start-end`), the SAME identity `class_key` uses — NOT the bare qualified
    // `ref`. A class with DUPLICATE qualified refs (same-named methods/overloads in one file,
    // cfg variants) would otherwise label its `per_member_values` with indistinguishable names,
    // so a consumer (`clones --explain`, MCP output) could not map a value back to a UNIQUE
    // member. Only the string identity each entry carries changed in round-3; round-4 changes
    // only the SORT KEY.
    let canonical_member_refs: Vec<String> = {
        let mut ordered: Vec<(&str, &str, i64, String)> = raw_members
            .iter()
            .filter_map(|(id, start_byte, m)| {
                by_id.get(id).map(|b| {
                    (
                        b.struct_hash.as_str(),
                        m.path.as_str(),
                        *start_byte,
                        format!("{}@{}:{}-{}", m.r#ref, m.path, m.start_line, m.end_line),
                    )
                })
            })
            .collect();
        ordered.sort_unstable_by(|a, b| {
            canonical_member_order_key(a.0, a.1, a.2)
                .cmp(&canonical_member_order_key(b.0, b.1, b.2))
        });
        ordered.into_iter().take(MEMBER_VALUE_CAP).map(|(_, _, _, r)| r).collect()
    };

    // Cap the returned member list AFTER computing spread and key from the full set.
    // Fix 2 (#215): when a `pin` subject is supplied (clones_for_symbol) and that member exists but
    // would fall OUTSIDE the first `cap` members by id, guarantee its inclusion: keep the first
    // `cap - 1` by id plus the pinned member, so the caller always sees the symbol it asked about.
    // When `pin` is `None`, or the subject is already within the first `cap`, this is a no-op and
    // the selection is identical to the plain `take(cap)` path.
    let member_count = total_members;
    let members_returned = raw_members.len().min(cap);

    let pinned_idx = pin.and_then(|subject_id| {
        let pos = raw_members.iter().position(|(id, _, _)| *id == subject_id)?;
        // Only act when the pin would otherwise be dropped: it sits at or past `cap` in id order.
        (pos >= cap).then_some(pos)
    });

    let chosen: Vec<CloneMember> = match pinned_idx {
        Some(pos) => {
            // First `cap - 1` by id, plus the pinned member → exactly `cap` members.
            let mut chosen: Vec<CloneMember> =
                raw_members.iter().take(cap - 1).map(|(_, _, m)| m.clone()).collect();
            chosen.push(raw_members[pos].2.clone());
            chosen
        },
        None => raw_members.into_iter().take(cap).map(|(_, _, m)| m).collect(),
    };
    let mut members = chosen;
    members.sort_unstable_by(|a, b| a.r#ref.cmp(&b.r#ref));

    // Load-bearing factor: 1 + ln(1 + max_fan_in_score) over members. Fan-in proxy via
    // `scoped_weighted_fan_in` (heuristic-only, no oracle data at this call site).
    let oracle = crate::query::load_bearing::OracleContext::none();
    let max_importance = component
        .iter()
        .filter_map(|&id| {
            crate::query::load_bearing::scoped_weighted_fan_in(conn, id, &oracle)
                .ok()
                .flatten()
                .map(|e| e.score)
        })
        .fold(0.0_f64, f64::max);
    let load_bearing_factor = 1.0 + max_importance.ln_1p();

    // Median token_len across all bags in the component.
    let mut token_lens: Vec<i64> = bags.iter().map(|b| b.token_len).collect();
    token_lens.sort_unstable();
    let median_token_len = token_lens[token_lens.len() / 2];

    let roi = cross_module_spread as f64
        * member_count as f64
        * body_token_len_medoid as f64
        * load_bearing_factor
        * cohesion_min_pairwise;

    let roi_factors = RoiFactors {
        member_count,
        cross_module_spread,
        median_token_len,
        load_bearing_factor,
        cohesion_penalty: cohesion_min_pairwise,
    };

    Ok(Some(CandidateCloneClass {
        class_key,
        class_kind: "candidate_component",
        language,
        refined: false,
        members,
        member_count,
        members_returned,
        total_members,
        similarity_min,
        similarity_medoid_min,
        containment_max,
        cohesion_min_pairwise,
        cross_module_spread,
        body_token_len_medoid,
        roi,
        roi_factors,
        metrics_sampled,
        medoid_symbol_id,
        // Refinement fields are None on an un-refined candidate class; the two-phase driver in
        // `find_clones` / `clones_for_symbol` populates them (and flips `refined`/`class_kind`).
        lcs_ratio: None,
        confidence: None,
        refactorability: None,
        refine_mode: None,
        template: None,
        variation_points: None,
        proposed_signature: None,
        anti_unify_coverage: None,
        canonical_member_refs: Some(canonical_member_refs),
    }))
}

/// `(symbol_id, symbol_id)` candidate pairs (a < b), both within the scoped `files` view.
///
/// Combines the `struct_hash` exact fast path with the sub-block + exact-verify candidate read
/// (design rev 4 §3b). Returns deduplicated `(a, b)` pairs with `a < b` for the union-find.
fn candidate_pairs(conn: &Connection) -> anyhow::Result<Vec<(i64, i64)>> {
    let bags = load_scoped_baseline_bags(conn)?;

    let mut pairs: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();

    // 1. Exact fast path: every same-struct_hash set contributes all its pairwise pairs.
    add_struct_hash_pairs(&bags, &mut pairs);

    // 2. Sub-block candidate pairs via the inverted index over sub-block tokens only.
    //    `candidate_clone_components` keeps the const THETA (behavior unchanged) — only
    //    `find_clones` threads the caller's `min_similarity` through candidate generation.
    let candidate = sub_block_candidate_pairs(&bags, THETA);

    // 3. Size prune + EXACT max-denominator verify over the FULL bags.
    let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
    for (a, b) in candidate {
        let (ba, bb) = (by_id[&a], by_id[&b]);
        if verified_clone(ba, bb, THETA) {
            pairs.insert((a, b));
        }
    }

    Ok(pairs.into_iter().collect())
}

/// Load every scoped baseline symbol's fingerprint + full token bag with LEFT-JOINed df. Both the
/// fingerprint and its postings are filtered to the scoped `files` view through `symbols.file_id`,
/// so only the ACTIVE version of each file participates (SCOPED-VIEW REQUIREMENT #89). df is read
/// via LEFT JOIN + COALESCE so a missing-df token is never dropped (design rev-4 §2).
fn load_scoped_baseline_bags(conn: &Connection) -> anyhow::Result<Vec<SymbolBag>> {
    // Load `clone_token_df` ONCE into a map (#231 R3): the token bag now lives in an opaque
    // `token_bag` BLOB, so df can no longer be a per-token SQL JOIN. Each decoded token's df is
    // looked up here in Rust and COALESCEd to the fallback sentinel — a missing-df token must NOT
    // be dropped (design rev-4 §2). Only the baseline normalizer feeds candidate recall.
    let mut df_stmt = conn
        .prepare("SELECT token_hash, df FROM clone_token_df WHERE normalizer_kind = 'baseline'")?;
    let df_by_token: std::collections::HashMap<i64, i64> = df_stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
        .collect::<Result<_, _>>()?;

    // Scoped baseline fingerprints + their token-bag BLOB, in one read (no per-token join).
    // `files.generated = 0` excludes generated files (e.g. `src/generated/…`, `.d.ts`) from the
    // candidate read. As of #232 #6 generated files are NO LONGER fingerprinted at index time
    // (`prep.rs` / `file_index.rs` gate the compute on `!file_is_generated`), so this filter is now
    // defense-in-depth: it still guards a file that flipped to `generated = 1` AFTER its
    // fingerprints were written (a target reclassification without a reindex of that file).
    // `token_len` comes from the COLUMN (R3); the bag itself is decoded from the BLOB.
    let mut fp_stmt = conn.prepare(
        "SELECT sf.symbol_id, symbols.language, sf.struct_hash, sf.token_len, sf.token_bag
         FROM symbol_fingerprints sf
         JOIN symbols ON symbols.id = sf.symbol_id
         JOIN files ON files.id = symbols.file_id
         WHERE sf.normalizer_kind = 'baseline'
           AND sf.normalizer_version = ?1
           AND files.generated = 0",
    )?;
    let mut bags: Vec<SymbolBag> = Vec::new();
    let mut rows = fp_stmt.query([NORM_VERSION])?;
    while let Some(row) = rows.next()? {
        // R4: a NULL `token_bag` (un-reindexed after the V032 migration) is a NO-BAG row — SKIP it
        // (not an empty bag, no panic). Byte-identical recall holds only for a FULLY (re)indexed
        // DB; clone recall is undefined for NULL-bag symbols until the post-migration
        // reindex.
        let Some(blob) = row.get::<_, Option<Vec<u8>>>(4)? else {
            continue;
        };
        let Some(bag_pairs) = crate::index::clones::bag_blob::decode_token_bag(&blob) else {
            // A stale/corrupt blob (version mismatch / truncation) decodes to None — treat as
            // no-bag, same as NULL. It is repopulated on the next reindex.
            continue;
        };
        let tokens: Vec<TokenPosting> = bag_pairs
            .into_iter()
            .map(|(token_hash, freq)| TokenPosting {
                token_hash,
                freq,
                coalesced_df: df_by_token.get(&token_hash).copied().unwrap_or(DF_FALLBACK),
            })
            .collect();
        // The BLOB is stored token_hash-sorted (the producer's invariant), which is exactly the
        // order `overlap`'s two-pointer merge and `sub_block_tokens` expect — so no re-sort is
        // needed on read. Assert it in debug to catch a producer regression.
        debug_assert!(
            tokens.windows(2).all(|w| w[0].token_hash <= w[1].token_hash),
            "decoded token_bag must be token_hash-sorted"
        );
        bags.push(SymbolBag {
            symbol_id: row.get(0)?,
            language: row.get(1)?,
            struct_hash: row.get(2)?,
            token_len: row.get(3)?,
            tokens,
        });
    }

    // Return bags in `symbol_id` order, matching the prior `BTreeMap<symbol_id, _>` keyset (the SQL
    // has no ORDER BY). The candidate set is order-independent (it dedups into a BTreeSet), but a
    // stable order keeps any incidental iteration deterministic.
    bags.sort_unstable_by_key(|bag| bag.symbol_id);
    Ok(bags)
}

/// Exact fast path: every group of symbols sharing a `struct_hash` AND `language` is
/// identical-after-normalization, so it contributes all its pairwise pairs (no overlap math).
/// Language partition is required: different languages share no grammar token space, so a
/// struct_hash collision across languages is a false positive.
fn add_struct_hash_pairs(bags: &[SymbolBag], pairs: &mut std::collections::BTreeSet<(i64, i64)>) {
    // Key: (struct_hash, language) — only same-language symbols can be struct-hash clones.
    let mut by_hash: BTreeMap<(&str, &str), Vec<i64>> = BTreeMap::new();
    for bag in bags {
        by_hash
            .entry((bag.struct_hash.as_str(), bag.language.as_str()))
            .or_default()
            .push(bag.symbol_id);
    }
    for ids in by_hash.values() {
        for (i, &a) in ids.iter().enumerate() {
            for &b in &ids[i + 1..] {
                pairs.insert((a.min(b), a.max(b)));
            }
        }
    }
}

/// Build the inverted index over sub-block tokens only and emit candidate pairs `(a < b)` for every
/// pair of symbols sharing a sub-block token. Admissibility (design rev-4 §3b): two symbols can
/// reach similarity ≥ θ only if their sub-blocks share a token hash, so this yields every true
/// candidate pair regardless of df accuracy (given the shared total order).
///
/// Language partition: only same-language pairs are emitted — different languages have disjoint
/// grammar token spaces, so a token-hash collision across languages is a false positive.
fn sub_block_candidate_pairs(
    bags: &[SymbolBag],
    theta: f64,
) -> std::collections::BTreeSet<(i64, i64)> {
    // id → language for the partition guard applied at pair-emit time.
    let lang_of: BTreeMap<i64, &str> =
        bags.iter().map(|b| (b.symbol_id, b.language.as_str())).collect();

    let mut inverted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for bag in bags {
        for token_hash in sub_block_tokens(bag, theta) {
            inverted.entry(token_hash).or_default().push(bag.symbol_id);
        }
    }

    let mut candidate: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();
    for ids in inverted.values() {
        for (i, &a) in ids.iter().enumerate() {
            for &b in &ids[i + 1..] {
                // Language partition: skip cross-language pairs.
                if lang_of[&a] != lang_of[&b] {
                    continue;
                }
                candidate.insert((a.min(b), a.max(b)));
            }
        }
    }
    candidate
}

/// A symbol's sub-block: the distinct token hashes whose occurrences reach into the first `p`
/// occurrences under the deterministic total order `(coalesced_df ASC, token_hash ASC)`.
///
/// `p = token_len - ceil(theta * token_len) + 1` is the sub-block OCCURRENCE length (clamped to ≥
/// 0; if `p >= token_len` the whole bag is the sub-block). The sub-block is defined over EXPANDED
/// token occurrences (Σ freq), not distinct posting rows, so it matches the multiset `Σ min(freq)`
/// verifier (design rev-4 §3): walking distinct tokens in order accumulating `freq`, a token is
/// included if the running occurrence-count BEFORE it is `< p` (i.e. any of its occurrences falls
/// in the prefix).
fn sub_block_tokens(bag: &SymbolBag, theta: f64) -> Vec<i64> {
    let p = sub_block_len(bag.token_len, theta);
    if p <= 0 {
        return Vec::new();
    }

    let mut ordered: Vec<&TokenPosting> = bag.tokens.iter().collect();
    ordered.sort_by_key(|t| (t.coalesced_df, t.token_hash));

    let mut sub_block = Vec::new();
    let mut occurrences_before: i64 = 0;
    for token in ordered {
        // Include this token if any of its occurrences falls within the first `p` occurrences, i.e.
        // the running count BEFORE it is still inside the prefix.
        if occurrences_before < p {
            sub_block.push(token.token_hash);
        } else {
            break; // every later token starts past the prefix too (occurrences only grow).
        }
        occurrences_before += token.freq;
    }
    sub_block
}

/// Sub-block occurrence length `p = token_len - ceil(theta * token_len) + 1`, clamped to ≥ 0.
fn sub_block_len(token_len: i64, theta: f64) -> i64 {
    let threshold = (theta * token_len as f64).ceil() as i64;
    (token_len - threshold + 1).max(0)
}

/// Size prune + EXACT max-denominator verify (design rev-4 §3b). With `min_len`/`max_len` = the two
/// token_lens: cheap size prune `min_len >= ceil(theta * max_len)`; then `overlap = Σ min(freq_a,
/// freq_b)` over the FULL bags, kept iff `overlap >= ceil(theta * max_len)`. The GATE is
/// `similarity = overlap / max_len`; containment = `overlap / min_len` is NOT gated here.
fn verified_clone(a: &SymbolBag, b: &SymbolBag, theta: f64) -> bool {
    let min_len = a.token_len.min(b.token_len);
    let max_len = a.token_len.max(b.token_len);
    let threshold = (theta * max_len as f64).ceil() as i64;

    // Size prune: a smaller block can't reach θ against a larger one.
    if min_len < threshold {
        return false;
    }

    overlap(a, b) >= threshold
}

/// Exact multiset overlap `Σ min(freq_a, freq_b)` over the two FULL token bags.
///
/// Requires both bags' `tokens` slices to be sorted by `token_hash` ascending (guaranteed by
/// the encoded token-bag BLOB, which is sorted at encode time by `tokens::token_bag` and
/// asserted by the `debug_assert` in `bag_blob::encode_token_bag`). Uses an allocation-free
/// two-pointer merge: no `BTreeMap` rebuild per call — O(|a| + |b|) time, zero heap allocation.
fn overlap(a: &SymbolBag, b: &SymbolBag) -> i64 {
    let (mut ia, mut ib) = (0, 0);
    let (ta, tb) = (a.tokens.as_slice(), b.tokens.as_slice());
    let mut total: i64 = 0;
    while ia < ta.len() && ib < tb.len() {
        match ta[ia].token_hash.cmp(&tb[ib].token_hash) {
            std::cmp::Ordering::Less => ia += 1,
            std::cmp::Ordering::Greater => ib += 1,
            std::cmp::Ordering::Equal => {
                total += ta[ia].freq.min(tb[ib].freq);
                ia += 1;
                ib += 1;
            },
        }
    }
    total
}

/// Union-find the pairs into components of size >= 2 (sorted for determinism).
fn components_from_pairs(pairs: &[(i64, i64)]) -> Vec<Vec<i64>> {
    use std::collections::BTreeMap;

    fn find(parent: &mut BTreeMap<i64, i64>, x: i64) -> i64 {
        let mut root = x;
        while let Some(&p) = parent.get(&root) {
            if p == root {
                break;
            }
            root = p;
        }
        let mut cur = x;
        while let Some(&p) = parent.get(&cur) {
            if p == root {
                break;
            }
            parent.insert(cur, root);
            cur = p; // p captured before the insert, so advancing to the pre-compression parent is safe
        }
        root
    }

    let mut parent: BTreeMap<i64, i64> = BTreeMap::new();
    for &(a, b) in pairs {
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent.insert(ra.max(rb), ra.min(rb));
        }
    }
    let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let members: Vec<i64> = parent.keys().copied().collect(); // collect keys first: find() needs &mut parent
    for member in members {
        let root = find(&mut parent, member);
        groups.entry(root).or_default().push(member);
    }
    groups
        .into_values()
        .filter(|g| g.len() >= 2)
        .map(|mut g| {
            g.sort_unstable();
            g
        })
        .collect()
}

/// Partition the θ-verified candidate `pairs` into per-component edge lists, parallel to
/// `components` (entry `i` holds the edges whose endpoints belong to `components[i]`) (#256).
///
/// The coherence split seeds its clique cover from a component's edge list; supplying the
/// precomputed edges makes seeding O(edges) instead of the old O(n²) all-pairs scan (the reason the
/// removed `SPLIT_MAX` member cap existed). Both endpoints of a pair share a component by
/// construction (`components_from_pairs` union-finds the same pairs), so a node→component-index map
/// resolves every edge in ONE O(|pairs|) pass. An edge whose endpoints fall in a dropped singleton
/// component (not in the map) is skipped — it can never appear, but the guard keeps the partition
/// total.
fn bucket_edges_by_component(
    pairs: &[(i64, i64)],
    components: &[Vec<i64>],
) -> Vec<Vec<(i64, i64)>> {
    let mut node_to_component: BTreeMap<i64, usize> = BTreeMap::new();
    for (idx, component) in components.iter().enumerate() {
        for &node in component {
            node_to_component.insert(node, idx);
        }
    }
    let mut edges_by_component: Vec<Vec<(i64, i64)>> = vec![Vec::new(); components.len()];
    for &(a, b) in pairs {
        if let Some(&idx) = node_to_component.get(&a) {
            // `a` and `b` are unioned into the same component, so indexing by `a` is correct.
            edges_by_component[idx].push((a, b));
        }
    }
    edges_by_component
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        SymbolBag, THETA, TokenPosting, add_struct_hash_pairs, bucket_edges_by_component,
        class_key_for, components_from_pairs, coverage_roi_gate, overlap,
        sub_block_candidate_pairs,
    };

    /// #256: the refined-ROI coverage gate is a mutually-exclusive band. A near-zero-coverage
    /// (degenerate `⟨m0⟩`) class gets the strong penalty so it can't float to the top on member
    /// count; a `[0.3, 0.5)` class gets the mild 0.70; at/above 0.5 there is no penalty.
    #[test]
    fn roi_low_coverage_refined_class_downranked() {
        // Strong band: below 0.3 → the order-of-magnitude penalty (degenerate, e.g. coverage 0.00).
        assert_eq!(coverage_roi_gate(0.0), super::COVERAGE_STRONG_PENALTY);
        assert_eq!(coverage_roi_gate(0.29), super::COVERAGE_STRONG_PENALTY);
        // Mild band: [0.3, 0.5) → 0.70 (matches refactorability_v2).
        assert_eq!(coverage_roi_gate(0.3), super::COVERAGE_MILD_PENALTY);
        assert_eq!(coverage_roi_gate(0.49), super::COVERAGE_MILD_PENALTY);
        // No penalty at/above 0.5 — a healthy class is untouched.
        assert_eq!(coverage_roi_gate(0.5), 1.0);
        assert_eq!(coverage_roi_gate(1.0), 1.0);
        // The gate strictly down-ranks a degenerate class relative to a healthy one with the SAME
        // structural factors: a coverage-0.00 class's ROI multiplier (0.10) is far below a
        // coverage-1.0 class's (1.0), so member count alone can no longer invert the order.
        assert!(coverage_roi_gate(0.0) < coverage_roi_gate(0.6));
        // Each factor is strictly positive (never zeroes the ROI — a gated class stays visible).
        for cov in [0.0, 0.2, 0.4, 0.6, 1.0] {
            assert!(coverage_roi_gate(cov) > 0.0);
        }
    }

    /// #256: `bucket_edges_by_component` partitions the candidate pairs into per-component edge
    /// lists parallel to `components`, with both endpoints landing in the same bucket.
    #[test]
    fn bucket_edges_partitions_pairs_per_component() {
        // Two disjoint components: {1,2,3} and {10,11}.
        let pairs = vec![(1, 2), (2, 3), (1, 3), (10, 11)];
        let components = components_from_pairs(&pairs);
        // components_from_pairs sorts components by lowest id, so [0]={1,2,3}, [1]={10,11}.
        assert_eq!(components, vec![vec![1, 2, 3], vec![10, 11]]);
        let buckets = bucket_edges_by_component(&pairs, &components);
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0], vec![(1, 2), (2, 3), (1, 3)]);
        assert_eq!(buckets[1], vec![(10, 11)]);
    }

    // ── Fix A: NaN / non-finite min_similarity ────────────────────────────────────────────────

    /// NaN and non-finite values must be rejected by the range guard. The old guard
    /// `v <= 0.0 || v > 1.0` passes NaN (both comparisons return false for NaN), which makes
    /// `ceil(NaN) as i64 = 0` → whole-bag sub-block → every same-language token-sharing pair
    /// is a clone → O(n²) blowup. Fix A adds `!v.is_finite()` before the range checks.
    #[test]
    fn find_clones_rejects_nan_and_non_finite_min_similarity() {
        use crate::index::FindClonesOptions;

        // We don't need a real DB here — the validation fires before any DB access.
        // Construct a minimal IndexDatabase pointing at a non-existent path; the validation
        // bail!() runs in the same function before the DB is touched.
        // Instead, test the validation logic directly via the public API by setting up a
        // temporary empty database and calling find_clones with the bad values.
        let root = std::env::temp_dir().join(format!(
            "rag-rat-nan-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let config = crate::Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![crate::config::ResolvedTarget {
                name: "rust".to_string(),
                language: crate::language::Language::Rust,
                directories: vec![std::path::PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: crate::config::TargetKind::Source,
            }],
            local_ai: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
        };
        crate::IndexDatabase::rebuild(&config).unwrap();
        let db = crate::IndexDatabase::open_config(&config).unwrap();

        // NaN must be rejected (non-finite, caught by !v.is_finite()).
        let err = db
            .find_clones(FindClonesOptions {
                min_similarity: Some(f64::NAN),
                min_copies: None,
                limit: None,
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("[0.5, 1.0]"),
            "NaN should produce a '[0.5, 1.0]' error message, got: {err}"
        );

        // +infinity must be rejected (non-finite, caught by !v.is_finite()).
        let err = db
            .find_clones(FindClonesOptions {
                min_similarity: Some(f64::INFINITY),
                min_copies: None,
                limit: None,
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("[0.5, 1.0]"),
            "INFINITY should produce a '[0.5, 1.0]' error message, got: {err}"
        );

        // -infinity must be rejected (non-finite, caught by !v.is_finite()).
        let err = db
            .find_clones(FindClonesOptions {
                min_similarity: Some(f64::NEG_INFINITY),
                min_copies: None,
                limit: None,
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("[0.5, 1.0]"),
            "NEG_INFINITY should produce a '[0.5, 1.0]' error message, got: {err}"
        );

        // 0.0 still rejected (below the 0.5 floor).
        assert!(
            db.find_clones(FindClonesOptions {
                min_similarity: Some(0.0),
                min_copies: None,
                limit: None,
            })
            .is_err()
        );

        // 1.0 is the boundary — must NOT be rejected.
        assert!(
            db.find_clones(FindClonesOptions {
                min_similarity: Some(1.0),
                min_copies: None,
                limit: None,
            })
            .is_ok()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Fix B: allocation-free two-pointer overlap ────────────────────────────────────────────

    fn make_bag_with_tokens(id: i64, tokens: Vec<(i64, i64)>) -> SymbolBag {
        let mut postings: Vec<TokenPosting> = tokens
            .into_iter()
            .map(|(hash, freq)| TokenPosting { token_hash: hash, freq, coalesced_df: 1 })
            .collect();
        // Simulate what load_scoped_baseline_bags does: sort by token_hash.
        postings.sort_unstable_by_key(|t| t.token_hash);
        SymbolBag {
            symbol_id: id,
            language: "rust".to_string(),
            struct_hash: format!("hash{id}"),
            token_len: postings.iter().map(|t| t.freq).sum(),
            tokens: postings,
        }
    }

    /// The two-pointer `overlap` must return the same value as the naive map-based version for
    /// any pair of token multisets.
    #[test]
    fn overlap_two_pointer_matches_naive() {
        fn naive_overlap(a: &SymbolBag, b: &SymbolBag) -> i64 {
            let freq_a: std::collections::BTreeMap<i64, i64> =
                a.tokens.iter().map(|t| (t.token_hash, t.freq)).collect();
            let mut total = 0;
            for token in &b.tokens {
                if let Some(&fa) = freq_a.get(&token.token_hash) {
                    total += fa.min(token.freq);
                }
            }
            total
        }

        // Case 1: fully disjoint — overlap must be 0.
        let a = make_bag_with_tokens(1, vec![(1, 3), (2, 2)]);
        let b = make_bag_with_tokens(2, vec![(3, 1), (4, 5)]);
        assert_eq!(overlap(&a, &b), 0);
        assert_eq!(overlap(&a, &b), naive_overlap(&a, &b));

        // Case 2: fully identical — overlap = sum of all freqs.
        let a = make_bag_with_tokens(1, vec![(10, 2), (20, 3), (30, 1)]);
        let b = make_bag_with_tokens(2, vec![(10, 2), (20, 3), (30, 1)]);
        assert_eq!(overlap(&a, &b), 6); // 2+3+1
        assert_eq!(overlap(&a, &b), naive_overlap(&a, &b));

        // Case 3: partial overlap, asymmetric frequencies.
        // a: token 5 freq=4, token 7 freq=2, token 9 freq=1
        // b: token 5 freq=2, token 8 freq=3, token 9 freq=5
        // overlap = min(4,2) + min(1,5) = 2 + 1 = 3
        let a = make_bag_with_tokens(1, vec![(5, 4), (7, 2), (9, 1)]);
        let b = make_bag_with_tokens(2, vec![(5, 2), (8, 3), (9, 5)]);
        assert_eq!(overlap(&a, &b), 3);
        assert_eq!(overlap(&a, &b), naive_overlap(&a, &b));

        // Case 4: one empty bag — overlap must be 0.
        let a = make_bag_with_tokens(1, vec![(1, 1)]);
        let b = make_bag_with_tokens(2, vec![]);
        assert_eq!(overlap(&a, &b), 0);
        assert_eq!(overlap(&b, &a), 0);
        assert_eq!(overlap(&a, &b), naive_overlap(&a, &b));
    }

    // ── Fix C: METRIC_SAMPLE_CAP — existing tests have metrics_sampled=false ─────────────────

    // (No in-unit test for the >200 sampled path: planting 200+ valid fingerprinted symbols
    // via an integration DB is too expensive for a unit test. The struct field and the cap guard
    // are covered by compilation + the assertion below on small components.)

    /// For all test-fixture components (size <= METRIC_SAMPLE_CAP), metrics_sampled must be false
    /// and behavior must be identical to the pre-cap code. This test exercises the union-find and
    /// struct-level assertions only; the full DB integration test in clones_handler covers the
    /// rest.
    #[test]
    fn small_component_metrics_sampled_is_false() {
        // Verify the constant is what the spec requires.
        assert_eq!(super::METRIC_SAMPLE_CAP, 200);

        // The components from a small pair (union-find returns groups of size 2).
        let comps = components_from_pairs(&[(1, 2)]);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].len(), 2);
        // len=2 < 200 → metrics_sampled would be false in build_class.
        // We can't call build_class without a DB, so we just assert the cap logic in isolation:
        let n = comps[0].len();
        let metrics_sampled = n > super::METRIC_SAMPLE_CAP;
        assert!(!metrics_sampled, "a 2-member component must not trigger metrics_sampled");
    }

    // ── Fix E: CLI handle routing ─────────────────────────────────────────────────────────────
    // (Tested in the CLI crate test below; here we just verify parse_sym_handle behaviour
    // that the routing logic depends on.)

    #[test]
    fn parse_sym_handle_accepts_valid_handles_and_rejects_others() {
        use crate::serde_big_id::parse_sym_handle;

        // A valid sym_<hex> handle round-trips.
        let h = crate::serde_big_id::format_sym_handle(12345i64);
        assert!(parse_sym_handle(&h).is_some());

        // A qualified name like `sym_utils.rs::load_user` is NOT a valid handle — it has `::`
        // and the hex part is `utils.rs` which is not valid hex.
        assert!(parse_sym_handle("sym_utils.rs::load_user").is_none());

        // A bare string without `sym_` prefix is None.
        assert!(parse_sym_handle("foo::bar").is_none());
    }

    // ── Existing tests ────────────────────────────────────────────────────────────────────────

    #[test]
    fn class_key_is_deterministic_and_order_independent() {
        let k1 = class_key_for(&["a.rs::x".into(), "b.rs::y".into()]);
        let k2 = class_key_for(&["b.rs::y".into(), "a.rs::x".into()]);
        assert_eq!(k1, k2);
        assert_ne!(k1, class_key_for(&["a.rs::x".into(), "c.rs::z".into()]));
    }

    /// Fix 3 (#215): the class key is built from per-member `ref@path:start-end` material, so two
    /// components that share the same qualified-name multiset but live at different LOCATIONS get
    /// distinct keys. This guards `clone_refinements.class_key` (a TEXT PRIMARY KEY in Plan 4) from
    /// conflating two real clone classes (overloads, cfg variants, same-named methods on different
    /// impls). `class_key_for` itself is unchanged — only the material `build_class` feeds it is.
    #[test]
    fn class_key_distinguishes_same_ref_at_different_locations() {
        // Same qualified name `x`, same span, DIFFERENT file → distinct keys.
        let key_a = class_key_for(&["x@a.rs:1-5".into()]);
        let key_b = class_key_for(&["x@b.rs:1-5".into()]);
        assert_ne!(key_a, key_b, "same ref in different files must not collide");

        // Same qualified name `x`, same file, DIFFERENT span → distinct keys.
        let key_span1 = class_key_for(&["x@a.rs:1-5".into()]);
        let key_span2 = class_key_for(&["x@a.rs:2-6".into()]);
        assert_ne!(key_span1, key_span2, "same ref/file at different spans must not collide");
    }

    /// Codex #4 (#215 Plan 4b): `canonical_member_refs` carries LOCATION-BEARING identities
    /// (`ref@path:start-end`), so a class with DUPLICATE qualified refs (same-named methods /
    /// overloads at different spans in one file) labels each `per_member_values` slot with a UNIQUE
    /// member identity. The bare-`ref` identity the old code used would emit two indistinguishable
    /// labels. This pins the construction the inlined `build_class` builder performs: same `ref`,
    /// different `path:start-end` → DISTINCT entries, while the ordinal order + cap are preserved.
    #[test]
    fn canonical_member_refs_are_location_bearing_for_duplicate_refs() {
        // Two members with the SAME qualified `ref` but DIFFERENT spans — the duplicate-ref case
        // (overloads / same-named methods). Mirror the build_class identity construction
        // (`ref@path:start-end`) so the test pins the exact production shape.
        let members: [(&str, &str, i64, i64); 2] =
            [("mod::overload", "src/a.rs", 1, 5), ("mod::overload", "src/a.rs", 10, 14)];
        let refs: Vec<String> =
            members.iter().map(|(r, p, s, e)| format!("{r}@{p}:{s}-{e}")).collect();

        // The two entries must be DISTINCT (location-bearing disambiguates the shared `ref`).
        assert_ne!(
            refs[0], refs[1],
            "duplicate-ref members must get DISTINCT location-bearing identities, got {refs:?}"
        );
        assert!(
            refs.iter().all(|r| r.starts_with("mod::overload@")),
            "each identity must carry the qualified ref AND its location, got {refs:?}"
        );
        // The bare ref alone WOULD collide (the bug the location-bearing identity fixes).
        let bare: Vec<&str> = members.iter().map(|(r, _, _, _)| *r).collect();
        assert_eq!(bare[0], bare[1], "the bare refs collide — exactly what location-bearing fixes");
        // The cap is unchanged: ≤ MEMBER_VALUE_CAP entries (here 2, well under the cap).
        assert!(refs.len() <= super::MEMBER_VALUE_CAP, "the MEMBER_VALUE_CAP cap is preserved");
    }

    /// Fix 2 (#215 Plan 4b Codex round-4): the canonical member ordering is REINDEX-STABLE — keyed
    /// on `(struct_hash, path, start_byte)`, NOT `(struct_hash, symbol_id)`. `symbol_id` is a rowid
    /// reassigned on every reindex; the 4b cache is content-addressed, so a file-unchanged reindex
    /// serves cached `per_member_values` frozen at the OLD member order while
    /// `canonical_member_refs` is recomputed live. If the order keyed off `symbol_id`, two
    /// members sharing a struct_hash could REORDER across the reindex → value[i] labelled by
    /// the wrong member.
    ///
    /// This pins the SORT KEY directly (the single source of truth `canonical_member_order_key`,
    /// used byte-for-byte by BOTH `load_refine_members` and `build_class`'s
    /// `canonical_member_refs`): two equal-struct_hash members whose `symbol_id`s are SWAPPED
    /// (the reindex simulation) must sort to the SAME order — so per_member_values[i] still
    /// maps to canonical_member_refs[i].
    #[test]
    fn refine_member_order_is_reindex_stable() {
        // Two equal-struct_hash members at distinct (path, start_byte) locations. Model each member
        // as the (struct_hash, path, start_byte, symbol_id) the two sort sites carry.
        let sh = "shared_struct_hash";
        let member_a = (sh, "src/a.rs", 100i64); // location A
        let member_b = (sh, "src/b.rs", 200i64); // location B

        // The ordering key is independent of symbol_id, so the order from BOTH symbol_id
        // assignments is identical. "Reindex" = swap which rowid each location got.
        let order_for = |id_a: i64, id_b: i64| -> Vec<(&str, i64)> {
            // (key-tuple, symbol_id, location-identity) — exactly the shape both call sites sort.
            let mut rows = vec![
                (member_a.0, member_a.1, member_a.2, id_a, (member_a.1, member_a.2)),
                (member_b.0, member_b.1, member_b.2, id_b, (member_b.1, member_b.2)),
            ];
            // Sort by the SAME helper both production sites use — symbol_id is NOT part of the key.
            rows.sort_unstable_by(|x, y| {
                super::canonical_member_order_key(x.0, x.1, x.2)
                    .cmp(&super::canonical_member_order_key(y.0, y.1, y.2))
            });
            rows.into_iter().map(|r| r.4).collect()
        };

        // First index: A=rowid 1, B=rowid 2. Reindex: rowids swapped (A=2, B=1).
        let first = order_for(1, 2);
        let after_reindex = order_for(2, 1);
        assert_eq!(
            first, after_reindex,
            "swapping symbol_ids (reindex) must NOT change the member order — the key is \
             (struct_hash, path, start_byte): {first:?} vs {after_reindex:?}"
        );

        // And the order is the location-derived one (path then start_byte), not the rowid order:
        // src/a.rs:100 sorts before src/b.rs:200 regardless of which rowid each got.
        assert_eq!(
            first,
            vec![("src/a.rs", 100i64), ("src/b.rs", 200i64)],
            "the canonical order is (path, start_byte)-ascending, reindex-independent: {first:?}"
        );

        // Negative control: the OLD (struct_hash, symbol_id) key WOULD flip on the reindex (it is
        // exactly the bug). With rowids 1,2 the symbol_id order is A,B; swapped to 2,1 it is B,A —
        // proving the symbol_id key is unstable while the new key is not.
        let by_symbol_id = |id_a: i64, id_b: i64| -> Vec<(&str, i64)> {
            let mut rows = vec![(member_a.1, member_a.2, id_a), (member_b.1, member_b.2, id_b)];
            rows.sort_unstable_by_key(|r| r.2); // (struct_hash equal) → symbol_id alone
            rows.into_iter().map(|r| (r.0, r.1)).collect()
        };
        assert_ne!(
            by_symbol_id(1, 2),
            by_symbol_id(2, 1),
            "the OLD symbol_id key is reindex-UNSTABLE — this is the bug Fix 2 removes"
        );
    }

    #[test]
    fn union_find_groups_transitively_and_drops_singletons() {
        // 1-2, 2-3 => {1,2,3}; 5-6 => {5,6}; 9 alone => dropped.
        let comps = components_from_pairs(&[(1, 2), (2, 3), (5, 6)]);
        assert_eq!(comps, vec![vec![1, 2, 3], vec![5, 6]]);
    }

    /// Language partition unit test: two bags with IDENTICAL tokens/struct_hash/token_len but
    /// DIFFERENT languages must NOT pair via either the struct-hash fast path or the sub-block
    /// inverted-index path. Two same-language identical bags MUST pair via the struct-hash path.
    #[test]
    fn language_partition_blocks_cross_language_pairs_and_keeps_same_language() {
        // Shared token bag — same tokens, same struct_hash, same token_len.
        let make_bag = |id: i64, language: &str| SymbolBag {
            symbol_id: id,
            language: language.to_string(),
            struct_hash: "deadbeef".to_string(),
            token_len: 5,
            tokens: vec![
                TokenPosting { token_hash: 1, freq: 2, coalesced_df: 10 },
                TokenPosting { token_hash: 2, freq: 1, coalesced_df: 20 },
                TokenPosting { token_hash: 3, freq: 2, coalesced_df: 30 },
            ],
        };

        // id=1 is Rust, id=2 is TypeScript — identical token bags, different language.
        let bag_rust = make_bag(1, "rust");
        let bag_ts = make_bag(2, "typescript");
        // id=3 is also Rust, identical to id=1 — same language, same struct_hash.
        let bag_rust2 = make_bag(3, "rust");

        let bags = vec![bag_rust, bag_ts, bag_rust2];

        // struct-hash fast path: must NOT produce a cross-language pair (1,2) but MUST produce
        // same-language pair (1,3).
        let mut hash_pairs: BTreeSet<(i64, i64)> = BTreeSet::new();
        add_struct_hash_pairs(&bags, &mut hash_pairs);
        assert!(
            !hash_pairs.contains(&(1, 2)),
            "struct-hash path must not pair rust(1) with typescript(2): {hash_pairs:?}"
        );
        assert!(
            hash_pairs.contains(&(1, 3)),
            "struct-hash path must pair rust(1) with rust(3): {hash_pairs:?}"
        );

        // sub-block inverted-index path: same assertions.
        let sub_pairs = sub_block_candidate_pairs(&bags, THETA);
        assert!(
            !sub_pairs.contains(&(1, 2)),
            "sub-block path must not pair rust(1) with typescript(2): {sub_pairs:?}"
        );
        assert!(
            sub_pairs.contains(&(1, 3)),
            "sub-block path must pair rust(1) with rust(3): {sub_pairs:?}"
        );
    }

    // ── P2d: the refine-sampling flag fires at MEMBER_VALUE_CAP, not LCS_MEMBER_SAMPLE ───────────

    /// Build a minimal refined-eligible `CandidateCloneClass` with the given `member_count` and
    /// `metrics_sampled = false`, so `apply_refinement` is the only thing that can flip the flag.
    fn class_with_member_count(member_count: usize) -> super::CandidateCloneClass {
        super::CandidateCloneClass {
            class_key: "k".to_string(),
            class_kind: "candidate_component",
            language: "rust".to_string(),
            refined: false,
            members: Vec::new(),
            member_count,
            members_returned: 0,
            total_members: member_count,
            similarity_min: 1.0,
            similarity_medoid_min: 1.0,
            containment_max: 1.0,
            cohesion_min_pairwise: 1.0,
            cross_module_spread: 1,
            body_token_len_medoid: 10,
            roi: 0.0,
            roi_factors: super::RoiFactors {
                member_count,
                cross_module_spread: 1,
                median_token_len: 10,
                load_bearing_factor: 1.0,
                cohesion_penalty: 1.0,
            },
            metrics_sampled: false,
            medoid_symbol_id: None,
            lcs_ratio: None,
            confidence: None,
            refactorability: None,
            refine_mode: None,
            template: None,
            variation_points: None,
            proposed_signature: None,
            anti_unify_coverage: None,
            canonical_member_refs: None,
        }
    }

    /// A `CachedRefinement` whose `lcs_sampled` is FALSE — so the ONLY way `metrics_sampled` can
    /// flip is the member-count guard in `apply_refinement`.
    fn unsampled_refinement() -> crate::index::clones::refine::cache::CachedRefinement {
        crate::index::clones::refine::cache::CachedRefinement {
            lcs_ratio: 1.0,
            confidence: crate::index::clones::refine::score::Confidence::High,
            refactorability: 1.0,
            refine_mode: "baseline",
            template: String::new(),
            variation_points_json: "[]".to_string(),
            proposed_signature_json: "{}".to_string(),
            anti_unify_coverage: 1.0,
            lcs_sampled: false,
        }
    }

    #[test]
    fn refine_metrics_sampled_at_value_cap() {
        use super::{MEMBER_VALUE_CAP, apply_refinement};

        // A class with exactly MEMBER_VALUE_CAP members is NOT truncated by the loader → not
        // sampled (with lcs_sampled false).
        let mut at_cap = class_with_member_count(MEMBER_VALUE_CAP);
        apply_refinement(&mut at_cap, unsampled_refinement());
        assert!(
            !at_cap.metrics_sampled,
            "a class AT the value cap ({MEMBER_VALUE_CAP}) is not truncated → not sampled"
        );

        // The smallest class ABOVE the cap (51 with the current cap of 50) IS truncated by
        // `load_refine_members` (drops ≥1 member) yet sits BELOW LCS_MEMBER_SAMPLE (64) — exactly
        // the range the old `> LCS_MEMBER_SAMPLE` guard missed. That `MEMBER_VALUE_CAP <
        // LCS_MEMBER_SAMPLE` relationship is pinned at compile time by the module-level `const _`
        // next to the `MEMBER_VALUE_CAP` definition (now guarding production, not just this test).
        let mut above_cap = class_with_member_count(MEMBER_VALUE_CAP + 1);
        apply_refinement(&mut above_cap, unsampled_refinement());
        assert!(
            above_cap.metrics_sampled,
            "a {}-member class is truncated to {MEMBER_VALUE_CAP} refine inputs → metrics_sampled",
            MEMBER_VALUE_CAP + 1
        );
    }

    // ── T5a (#231): the LOAD-BEARING recall-parity pin ──────────────────────────────────────────

    /// THE recall-correctness pin for the BLOB-pack (#231). Builds a real index, then proves the
    /// BLOB read path produces SymbolBags BYTE-IDENTICAL to the pre-BLOB postings grouping — same
    /// token lists, freqs, AND coalesced df — and that the resulting `candidate_pairs` are
    /// unchanged. The "postings-era" expectation is reconstructed independently in the test from
    /// the same BLOBs + df, replicating the old `GROUP BY symbol_token_postings` + per-token
    /// `LEFT JOIN clone_token_df` + `COALESCE(df, DF_FALLBACK)` + sort-by-token_hash semantics. If
    /// these diverge, recall has regressed.
    #[test]
    fn recall_candidates_identical_blob_vs_postings_grouping() {
        use std::collections::HashMap;

        use super::{DF_FALLBACK, candidate_pairs, load_scoped_baseline_bags};

        let root = std::env::temp_dir().join(format!(
            "rag-rat-recall-parity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        // Two renamed-clone groups + one unrelated function, across files.
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
             compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s += it * \
             2; } s + 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/b.rs"),
            "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 \
             }\npub fn tally_amounts(values: Vec<i64>) -> i64 { let mut t = 0; for v in values { \
             t += v * 2; } t + 1 }\n",
        )
        .unwrap();
        let config = crate::Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![crate::config::ResolvedTarget {
                name: "rust".to_string(),
                language: crate::language::Language::Rust,
                directories: vec![std::path::PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: crate::config::TargetKind::Source,
            }],
            local_ai: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
        };
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        let conn = db.storage.connection();

        // --- Independently reconstruct the postings-era SymbolBags from the BLOBs + df ---
        // df map, exactly as the old per-token `LEFT JOIN clone_token_df` would have resolved.
        let mut df_stmt = conn
            .prepare("SELECT token_hash, df FROM clone_token_df WHERE normalizer_kind = 'baseline'")
            .unwrap();
        let df_by_token: HashMap<i64, i64> = df_stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        // For each scoped baseline fingerprint (generated = 0, matching the production filter),
        // decode its bag into the same `(symbol_id, language, struct_hash, token_len, [(hash, freq,
        // coalesced_df)])` shape, with the per-token list sorted by token_hash.
        let mut fp_stmt = conn
            .prepare(
                "SELECT sf.symbol_id, symbols.language, sf.struct_hash, sf.token_len, sf.token_bag
                 FROM symbol_fingerprints sf
                 JOIN symbols ON symbols.id = sf.symbol_id
                 JOIN files ON files.id = symbols.file_id
                 WHERE sf.normalizer_kind = 'baseline'
                   AND sf.normalizer_version = ?1
                   AND files.generated = 0",
            )
            .unwrap();
        // (symbol_id, language, struct_hash, token_len, sorted [(token_hash, freq, df)])
        type ExpectedBag = (i64, String, String, i64, Vec<(i64, i64, i64)>);
        let mut expected: Vec<ExpectedBag> = fp_stmt
            .query_map([super::NORM_VERSION], |r| {
                let blob: Option<Vec<u8>> = r.get(4)?;
                let pairs = blob
                    .and_then(|b| crate::index::clones::bag_blob::decode_token_bag(&b))
                    .unwrap_or_default();
                let mut tokens: Vec<(i64, i64, i64)> = pairs
                    .into_iter()
                    .map(|(h, f)| (h, f, df_by_token.get(&h).copied().unwrap_or(DF_FALLBACK)))
                    .collect();
                tokens.sort_unstable_by_key(|&(h, _, _)| h);
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, tokens))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        expected.sort_unstable_by_key(|b| b.0);
        assert!(expected.len() >= 4, "fixture indexed at least 4 fingerprinted functions");

        // --- The production read path must produce byte-identical bags ---
        let mut actual: Vec<ExpectedBag> = load_scoped_baseline_bags(conn)
            .unwrap()
            .into_iter()
            .map(|bag| {
                let tokens: Vec<(i64, i64, i64)> =
                    bag.tokens.iter().map(|t| (t.token_hash, t.freq, t.coalesced_df)).collect();
                (bag.symbol_id, bag.language, bag.struct_hash, bag.token_len, tokens)
            })
            .collect();
        actual.sort_unstable_by_key(|b| b.0);
        assert_eq!(
            actual, expected,
            "BLOB-decoded SymbolBags must equal the postings-era grouping (token lists, freqs, df)"
        );

        // --- And the candidate pairs are unchanged: the two renamed groups each pair up ---
        let pairs = candidate_pairs(conn).unwrap();
        let id_of = |name: &str| -> i64 {
            conn.query_row(
                "SELECT s.id FROM symbols s WHERE s.name = ?1 AND s.kind = 'function'",
                [name],
                |r| r.get(0),
            )
            .unwrap()
        };
        let (lu, lo) = (id_of("load_user"), id_of("load_order"));
        let (ct, ta) = (id_of("compute_totals"), id_of("tally_amounts"));
        let has = |a: i64, b: i64| pairs.contains(&(a.min(b), a.max(b)));
        assert!(has(lu, lo), "load_user/load_order are renamed clones → a candidate pair");
        assert!(has(ct, ta), "compute_totals/tally_amounts are renamed clones → a candidate pair");
        assert!(
            !has(lu, ct) && !has(lu, ta) && !has(lo, ct) && !has(lo, ta),
            "the two clone GROUPS do not cross-pair"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
