//! Read layer for clone detection (#215) — module index.
//!
//! Plan 1 shipped the candidate-component read that proves the fingerprint substrate; the
//! `find_clones` / `clones_for_symbol` surface is Plan 2; the coherence split + anti-unification
//! refinement is Plan 4. The concerns are split across cohesive sibling modules:
//!
//! - [`types`] — the public DTO / options / eligibility types crossing the MCP/CLI serde boundary.
//! - [`substrate`] — [`SymbolBag`] / [`TokenPosting`] and the SourcererCC candidate-pair algorithm
//!   (`struct_hash` fast path, sub-block filter, EXACT max-denominator verify, union-find).
//! - [`scoring`] — ROI gates, the un-refined member-count dampen, refinement application, and the
//!   shared canonical-order / completeness helpers.
//! - [`build`] — component → [`CandidateCloneClass`] hydration + the staleness signal.
//! - [`refine_load`] — the persisted-row hydration the refine driver reads before re-parsing.
//! - [`resolve`] — `clones_for_symbol` selector resolution + ineligibility classification.
//! - [`precompute`] — the background clone-edge-graph precompute (#286) + the `find_clones` fast
//!   path that reads it.
//! - [`of_text`] — clone-check of arbitrary not-yet-indexed text (#287).
//!
//! This file is the curated index: it declares the submodules, owns the shared caps/thresholds that
//! several stages key off, re-exports the crate/pub surface, and hosts the `impl IndexDatabase`
//! query methods (`candidate_clone_components`, `clone_symbol_refs`, `find_clones`,
//! `clones_for_symbol`, `load_refine_members`) that orchestrate the sibling stages. Keeping the
//! methods on the type means every call site is unchanged (the compiler verifies completeness).
//!
//! The candidate read is the SourcererCC algorithm (design rev 4 §3b): a `struct_hash` exact fast
//! path, a deterministic-total-order sub-block filter over scoped baseline postings, and an EXACT
//! max-denominator overlap verify. df is a *selectivity hint only* — admissibility comes from the
//! shared total order plus the exact verify, so a missing/stale df never drops a true clone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use rusqlite::Connection;

mod build;
mod refine_load;
mod resolve;
mod scoring;
mod substrate;
mod types;

/// The background clone-edge-graph precompute (#286): the resumable, generation-staged build that
/// persists `clone_edges`, plus (Phase C) the `find_clones` fast path that reads it. A child module
/// so it can reuse this module's private candidate-gen primitives (`load_scoped_baseline_bags`,
/// `sub_block_tokens`, `overlap`, `verified_clone`) — guaranteeing the persisted set equals the
/// live `candidate_pairs_from_bags` set.
pub(crate) mod precompute;

/// Clone-check of arbitrary not-yet-indexed text (#287): `clones_of_text` fingerprints the
/// functions in a string and finds their exact + near clones among the indexed symbols — a child
/// module so it can reuse this module's private candidate-gen primitives.
pub(crate) mod of_text;

#[cfg(test)]
mod tests;

pub use types::{
    CandidateCloneClass, CloneCompleteness, CloneEligibility, CloneIneligibilityReason,
    CloneMember, CloneSymbolSelector, ClonesForSymbolResult, FindClonesOptions, FindClonesResult,
    RoiFactors,
};

// ── Shared caps / thresholds ────────────────────────────────────────────────────────────────────

/// Similarity threshold θ: a candidate pair is kept iff `overlap / max_len >= THETA`. The MAX
/// denominator is deliberate (design rev-4 §3b) — it bounds the member length ratio to ≈1/θ, the
/// whole-symbol bias, so a tiny helper contained in a giant function (overlap/min ≈ 1.0) is NOT a
/// clone. Tunable later via the query surface.
pub(crate) const THETA: f64 = 0.7;

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

/// Member-hydration batch size for the `symbols.id IN (…)` query in [`build::build_class`]. SQLite
/// caps the number of host parameters per prepared statement (`SQLITE_MAX_VARIABLE_NUMBER` — 999 on
/// older system libs that the non-bundled `rusqlite` may link against), so a component larger than
/// that floor would fail `conn.prepare` outright. Hydrating in chunks of [`HYDRATION_CHUNK`] keeps
/// every statement well under the limit; results are accumulated then re-sorted by `symbol_id` so
/// the full population is processed deterministically regardless of chunk boundaries.
pub(crate) const HYDRATION_CHUNK: usize = 900;

use self::build::{build_class, count_stale_member_paths};
use self::refine_load::{load_refine_rows, load_source_discriminators};
use self::resolve::{classify_ineligibility_reason, resolve_selector_to_symbol_id};
use self::scoring::{
    apply_refinement, build_completeness, canonical_member_order_key,
    dampen_unrefined_member_count, min_pairwise_cohesion,
};
use self::substrate::{
    SymbolBag, bucket_edges_by_component, components_from_pairs, load_scoped_baseline_bags,
    overlap, pairs_for_query, subject_component_bfs,
};
use crate::index::IndexDatabase;
use crate::index::clones::refine::cache::{
    refine_compute_and_store_budgeted, refine_lookup, refinement_key,
};
use crate::index::clones::refine::split::coherence_split;

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
        let pairs = substrate::candidate_pairs(conn)?;
        Ok(components_from_pairs(&pairs))
    }

    /// The SORTED, UNCAPPED set of symbol refs (`path::name`) that are in ANY coherent clone class
    /// (≥ `min_copies` members) over the active scope — the symbol-level recall signal for the #279
    /// harness. `find_clones`'s per-class member list is capped at [`MAX_MEMBERS`], so the union of
    /// its returned members UNDERCOUNTS symbols in large classes (the #282 covering-subset yields
    /// 100+-member families); this counts EVERY member of every coherent class. Runs the SAME
    /// candidate → component → `coherence_split` pipeline as `find_clones`, but collects the
    /// uncapped member ids instead of building/refining/capping classes, then resolves them to
    /// refs. Two builds' outputs diff with plain `diff`: a removed ref is a symbol that stopped
    /// being a clone (a real recall regression) — unlike the class-level recall signature,
    /// which legitimately changes when clustering granularity changes.
    pub fn clone_symbol_refs(
        &self,
        min_similarity: Option<f64>,
        min_copies: Option<usize>,
    ) -> anyhow::Result<Vec<String>> {
        let conn = self.storage.connection();
        // Validate θ the same way `find_clones` does (see its doc): a ratio in [0.5, 1.0].
        if let Some(v) = min_similarity
            && (!v.is_finite() || !(0.5..=1.0).contains(&v))
        {
            anyhow::bail!("min_similarity must be in [0.5, 1.0]");
        }
        let bags = load_scoped_baseline_bags(conn)?;
        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
        let theta = min_similarity.unwrap_or(THETA);
        let min_copies = min_copies.unwrap_or(2);
        let pairs = pairs_for_query(conn, &bags, theta)?;
        let components = components_from_pairs(&pairs);
        let edges_by_component = bucket_edges_by_component(&pairs, &components);

        // Union of EVERY coherent class's uncapped member ids (mirrors find_clones's split loop).
        let mut symbol_ids: BTreeSet<i64> = BTreeSet::new();
        for (comp_idx, component) in components.iter().enumerate() {
            if component.len() < min_copies {
                continue;
            }
            let coherent_classes =
                coherence_split(component, &edges_by_component[comp_idx], |a, b| {
                    let ba = by_id[&a];
                    let bb = by_id[&b];
                    let max_len = ba.token_len.max(bb.token_len);
                    if max_len == 0 { 1.0 } else { overlap(ba, bb) as f64 / max_len as f64 }
                });
            for class in &coherent_classes {
                if class.len() >= min_copies {
                    symbol_ids.extend(class.iter().copied());
                }
            }
        }

        // Resolve ids → qualified-name refs (`path::name`) via `name_strings` — the SAME source as
        // `CloneMember.ref`. Chunked to respect SQLite's bound-parameter limit.
        let ids: Vec<i64> = symbol_ids.into_iter().collect();
        let mut refs: Vec<String> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(HYDRATION_CHUNK) {
            let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT ns.value FROM symbols
                 JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                 WHERE symbols.id IN ({})",
                placeholders.join(", ")
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                row.get::<_, String>(0)
            })?;
            for r in rows {
                refs.push(r?);
            }
        }
        refs.sort_unstable();
        refs.dedup();
        Ok(refs)
    }

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
        let pairs = pairs_for_query(conn, &bags, theta)?;
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
            let coherent_classes =
                coherence_split(component, &edges_by_component[comp_idx], |a, b| {
                    let ba = by_id[&a];
                    let bb = by_id[&b];
                    let max_len = ba.token_len.max(bb.token_len);
                    if max_len == 0 { 1.0 } else { overlap(ba, bb) as f64 / max_len as f64 }
                });
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

        // GLOBAL (cross-class) refine cost budget (#272). The per-class
        // `LCS_AGGREGATE_CELLS_BUDGET` / `ALIGN_AGGREGATE_CELLS_BUDGET` (~100M cells each)
        // bound ONE class's exact-DP work, but 50 classes each near their per-class cap
        // summed to ~48–52 s cold (paid TWICE under the RW busy-retry dispatcher). This
        // shared allowance bounds the WHOLE refine pass: each cold class draws its
        // per-class budget from `min(<lane const>, remaining)` and decrements the
        // counter, so once it is exhausted later classes degrade-and-sample (their
        // `metrics_sampled` latches) instead of running full exact DP. Warm cache hits
        // never touch it (they bypass the compute path). DETERMINISM: classes refine in a
        // fixed provisional-ROI order and each lane consumes deterministically, so the same
        // input truncates at the same class/pair. The allowance is sized so an all-cold
        // pass stays well under the MCP timeout; a single class can still spend up to its
        // full per-class budget when the global counter is fresh.
        const GLOBAL_REFINE_CELLS_BUDGET: u64 = 600_000_000;
        let mut global_refine_cells: u64 = GLOBAL_REFINE_CELLS_BUDGET;

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
                self.refine_class_in_place(
                    conn,
                    class_ids,
                    &by_id,
                    class,
                    &mut global_refine_cells,
                )?;
            }
            // #259 (Adversary C): dampen the member_count factor of every class that is STILL
            // un-refined after the refine pass — a within-budget class whose refinement was a no-op
            // (drifted source, overlay scope, parse failure, vanished hydration row). Its Plan-2
            // ROI is LINEAR in member_count and carries no coverage gate, so a large
            // refine-failed component would otherwise out-rank the (coverage-gated)
            // refined classes it is re-sorted against. Run AFTER refine (so
            // `class.refined` is final) and BEFORE the sort (so it affects ranking). A
            // refined class is left untouched.
            for (_class_ids, class) in built.iter_mut() {
                if !class.refined {
                    dampen_unrefined_member_count(class);
                }
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
                self.refine_class_in_place(
                    conn,
                    class_ids,
                    &by_id,
                    class,
                    &mut global_refine_cells,
                )?;
            }
            // #259 (Adversary C): dampen the member_count factor of EVERY class that is still
            // un-refined before the final re-sort — both the budget tail (idx ≥
            // UNLIMITED_REFINE_BUDGET, never refined) AND any within-budget class whose refinement
            // was a no-op (drifted source, overlay scope, parse failure, vanished hydration row).
            // Their Plan-2 ROI is LINEAR in member_count and carries no coverage gate, so a large
            // refine-failed component would otherwise out-rank a gated refined class purely on
            // size. Run AFTER refine (so `class.refined` is final) and BEFORE the sort
            // (so it affects ranking). A refined class is left untouched (its ROI
            // already went through the coverage gate).
            for (_class_ids, class) in built.iter_mut() {
                if !class.refined {
                    dampen_unrefined_member_count(class);
                }
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

        let freshness = self.repo_meta("git_commit")?.unwrap_or_else(|| "unknown".to_string());

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
        // SHARED cross-class cell allowance (#272): the cold compute path draws each lane's
        // per-class budget from this and decrements it, so the WHOLE `find_clones` refine pass is
        // cell-bounded. Warm cache hits return BEFORE the compute, so they never consume it.
        global_refine_cells: &mut u64,
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
        let refinement = refine_compute_and_store_budgeted(
            conn,
            &key,
            &class.language,
            &members,
            class.similarity_min,
            class.medoid_symbol_id,
            Some(global_refine_cells),
        )?;
        apply_refinement(class, refinement);

        Ok(())
    }

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

        // `eligibility` is the single source of truth; `symbol_resolved` / `symbol_fingerprinted`
        // are derived from it so the bool fields and the richer enum can never disagree (#274 3a).
        let make_result = |class: Option<CandidateCloneClass>,
                           eligibility: CloneEligibility,
                           stale_members: usize,
                           freshness: String| {
            let symbol_resolved = !matches!(eligibility, CloneEligibility::SymbolNotResolved);
            let symbol_fingerprinted = matches!(eligibility, CloneEligibility::Eligible);
            // A single class can only truncate by capping its own member list; there is no
            // class-limit here, so reuse the same member-cap signal as `find_clones`.
            let truncated = class.as_ref().is_some_and(|c| c.members_returned < c.total_members);
            ClonesForSymbolResult {
                class,
                symbol_resolved,
                symbol_fingerprinted,
                eligibility,
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

        let freshness = self.repo_meta("git_commit")?.unwrap_or_else(|| "unknown".to_string());

        let resolved_id = resolve_selector_to_symbol_id(conn, &selector)?;
        let Some(symbol_id) = resolved_id else {
            return Ok(make_result(None, CloneEligibility::SymbolNotResolved, 0, freshness));
        };

        let bags = load_scoped_baseline_bags(conn)?;
        // If the resolved symbol has no current-version fingerprint row it is not eligible — it
        // can't be in any clone class. Classify WHY (generated file, non-function kind,
        // stale normalizer_version, or below MIN_TOKENS) instead of collapsing all four
        // into one bool.
        if !bags.iter().any(|b| b.symbol_id == symbol_id) {
            let reason = classify_ineligibility_reason(conn, symbol_id)?;
            return Ok(make_result(None, CloneEligibility::Ineligible { reason }, 0, freshness));
        }

        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
        // #270 (+ PR #384 review): drill into the SAME clone graph `find_clones` reads, or a
        // stale-but-eligible PUBLISHED graph could make reverse lookup disagree with the listing.
        // So mirror `pairs_for_query`'s source choice exactly:
        //  - persisted graph eligible (the documented "mildly-stale-OK" fast path) → find the
        //    subject's component from THOSE pairs, identical to `find_clones`;
        //  - NO eligible persisted graph (a live recompute) → BFS only the subject's component
        //    (#270's win); both paths recompute the same live graph, so they stay consistent.
        let (component, component_edges) =
            match precompute::precomputed_pairs_if_eligible(conn, &by_id, THETA)? {
                Some(pairs) => {
                    let components = components_from_pairs(&pairs);
                    let Some(comp_idx) = components.iter().position(|c| c.contains(&symbol_id))
                    else {
                        return Ok(make_result(None, CloneEligibility::Eligible, 0, freshness));
                    };
                    let mut edges_by_component = bucket_edges_by_component(&pairs, &components);
                    (
                        components[comp_idx].clone(),
                        std::mem::take(&mut edges_by_component[comp_idx]),
                    )
                },
                None => {
                    // Live recompute: BFS the subject's component + its θ-edges directly, instead
                    // of generating EVERY pair + EVERY component. Same
                    // (component, edges) as the full live scan (equivalence
                    // test pins it). A component < 2 means the subject cohered
                    // with no peer → it is in no clone class (the `.position`-miss analog).
                    let (comp, edges) = subject_component_bfs(&by_id, symbol_id, THETA);
                    if comp.len() < 2 {
                        return Ok(make_result(None, CloneEligibility::Eligible, 0, freshness));
                    }
                    (comp, edges)
                },
            };

        // Plan 4a: coherence-split the component, then serve the coherent sub-class that contains
        // the subject. If the subject is a SINGLETON after the split (it cohered with no peer at
        // θ), there is NO whole-component fallback (#256): serving the full over-merged component
        // would re-expose the very giant the split exists to break. `clones_for_symbol` returns the
        // subject's COHERENT neighborhood (the clique(s) containing it) or nothing.
        let coherent_classes = coherence_split(&component, &component_edges, |a, b| {
            let ba = by_id[&a];
            let bb = by_id[&b];
            let max_len = ba.token_len.max(bb.token_len);
            if max_len == 0 { 1.0 } else { overlap(ba, bb) as f64 / max_len as f64 }
        });
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
                // `clones_for_symbol` refines exactly ONE class, so it is not subject to the
                // cross-class throttle (#272): seed a fresh full per-class allowance so its single
                // class always gets the whole `min(<lane const>, remaining)` = the lane const,
                // identical to the pre-#272 single-class behavior.
                let mut single_class_cells: u64 = u64::MAX;
                if let Some(c) = built.as_mut() {
                    self.refine_class_in_place(
                        conn,
                        &subclass,
                        &by_id,
                        c,
                        &mut single_class_cells,
                    )?;
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

        Ok(make_result(class, CloneEligibility::Eligible, stale_members, freshness))
    }

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
        // canonical_member_refs[i]. `(path, start_byte)` uniquely identifies
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
            let (seq, node_spans) = crate::index::clones::normalize::normalize_baseline_spanned(
                node,
                &text,
                row.language,
            );

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
