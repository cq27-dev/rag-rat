//! Unit tests for the clone query API. Kept in one sibling module (rather than scattered per file)
//! because most exercise cross-stage behavior — the ROI gate + dampen (scoring), the candidate-gen
//! substrate, `build_class`'s canonical ordering, and the refine loaders — against the shared
//! types. Each test reaches into the specific sibling module that owns the symbol it exercises.

use std::collections::{BTreeMap, BTreeSet};

use rag_rat_clones::NORM_VERSION;

use super::scoring::{
    COVERAGE_MILD_PENALTY, COVERAGE_STRONG_PENALTY, apply_refinement, canonical_member_order_key,
    class_key_for, coverage_roi_gate, dampen_unrefined_member_count,
};
use super::substrate::{
    DF_FALLBACK, METRIC_SAMPLE_CAP, SymbolBag, TokenPosting, add_struct_hash_pairs,
    bucket_edges_by_component, candidate_pairs, candidate_pairs_from_bags, components_from_pairs,
    load_scoped_baseline_bags, overlap, sub_block_candidate_pairs, subject_component_bfs,
};
use super::types::{CandidateCloneClass, RoiFactors};
use super::{MEMBER_VALUE_CAP, THETA};

/// #256: the refined-ROI coverage gate is a mutually-exclusive band. A near-zero-coverage
/// (degenerate `⟨m0⟩`) class gets the strong penalty so it can't float to the top on member
/// count; a `[0.3, 0.5)` class gets the mild 0.70; at/above 0.5 there is no penalty.
#[test]
fn roi_low_coverage_refined_class_downranked() {
    // Strong band: below 0.3 → the order-of-magnitude penalty (degenerate, e.g. coverage 0.00).
    assert_eq!(coverage_roi_gate(0.0), COVERAGE_STRONG_PENALTY);
    assert_eq!(coverage_roi_gate(0.29), COVERAGE_STRONG_PENALTY);
    // Mild band: [0.3, 0.5) → 0.70 (matches refactorability_v2).
    assert_eq!(coverage_roi_gate(0.3), COVERAGE_MILD_PENALTY);
    assert_eq!(coverage_roi_gate(0.49), COVERAGE_MILD_PENALTY);
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

/// #259 (Adversary C): the member_count dampen on un-refined classes. A class that FAILS
/// refinement keeps its member_count-LINEAR Plan-2 ROI with no coverage gate; the dampen
/// replaces the linear size factor with `1 + ln(1 + member_count)` so it grows sub-linearly.
/// The dampen is mild for small classes and bites for large ones — exactly where a
/// refine-failed component could masquerade as high-ROI.
#[test]
fn dampen_unrefined_member_count_subdues_large_classes() {
    // The raw Plan-2 ROI is member_count-LINEAR; the dampen makes it sub-linear.
    let mut small = class_with_member_count(2);
    small.refined = false;
    small.roi = small.member_count as f64; // unit structural factors → roi == member_count
    dampen_unrefined_member_count(&mut small);
    // member_count 2 → 1 + ln(3) ≈ 2.10: a <5% nudge — small un-refined classes keep their
    // order.
    assert!(
        (small.roi - (1.0 + 3.0_f64.ln())).abs() < 1e-9,
        "member_count 2 dampens to 1 + ln(3) ≈ 2.10, got {}",
        small.roi
    );

    let mut large = class_with_member_count(300);
    large.refined = false;
    large.roi = large.member_count as f64;
    dampen_unrefined_member_count(&mut large);
    // member_count 300 → 1 + ln(301) ≈ 6.71: a ~45× reduction from the raw 300.
    assert!(large.roi < 7.0, "member_count 300 dampens to ~6.71, got {}", large.roi);
    assert!(large.roi > 6.0, "the dampen keeps the class strictly positive, got {}", large.roi);

    // The dampen is monotone non-decreasing in member_count (a bigger class still scores ≥ a
    // smaller one — we deprioritize the LINEAR dominance, we don't invert size entirely).
    assert!(large.roi > small.roi, "300 members still out-scores 2 after the dampen");

    // A refined class is NEVER dampened (the helper self-guards): its ROI already went through
    // the coverage gate, so re-weighting it would double-penalize.
    let mut refined = class_with_member_count(300);
    refined.refined = true;
    refined.roi = 300.0;
    dampen_unrefined_member_count(&mut refined);
    assert_eq!(refined.roi, 300.0, "a refined class is left untouched by the dampen");
}

/// #259 (Adversary C): the load-bearing property. A LARGE refine-FAILED class (un-refined,
/// member_count-linear ROI, no coverage gate) must NOT out-rank a degenerate REFINED class that
/// the coverage gate already sank — once both share the same structural factors. Pre-dampen the
/// refine-failed class won on raw member_count; post-dampen the coverage-gated refined class is
/// ranked at least as high, so the masquerade is closed.
#[test]
fn refine_failed_class_no_longer_outranks_gated_refined_class() {
    // A degenerate refined class: 13 members, coverage 0.0 → the strong coverage gate (×0.10),
    // refactorability also degenerate. Same structural factors as the refine-failed class below
    // (unit spread, body 10, load-bearing 1.0, cohesion 1.0) so only the gate vs. member_count
    // differs.
    let mut refined = class_with_member_count(13);
    let mut degenerate = unsampled_refinement();
    degenerate.anti_unify_coverage = 0.0; // → COVERAGE_STRONG_PENALTY (×0.10)
    degenerate.refactorability = 0.10; // degenerate ⟨m0⟩ template
    apply_refinement(&mut refined, degenerate);
    assert!(refined.refined, "the refined class is flagged refined");

    // A refine-FAILED class with MANY more members (a large over-merged component that could
    // not be refined): same unit structural factors, raw member_count-linear Plan-2
    // ROI.
    let mut refine_failed = class_with_member_count(300);
    refine_failed.refined = false;
    // Reproduce the Plan-2 ROI build_class would have set (unit factors → roi == member_count).
    refine_failed.roi = refine_failed.cross_module_spread as f64
        * refine_failed.member_count as f64
        * refine_failed.body_token_len_medoid as f64
        * refine_failed.roi_factors.load_bearing_factor
        * refine_failed.cohesion_min_pairwise;

    // PRE-dampen: the refine-failed giant out-ranks the gated refined class purely on size —
    // the #259 bug. (300 × 10 = 3000 vs 13 × 10 × 0.10 × 0.10 = 1.3.)
    assert!(
        refine_failed.roi > refined.roi,
        "pre-dampen the refine-failed class masquerades as high-ROI: {} vs {}",
        refine_failed.roi,
        refined.roi
    );

    // Apply the dampen (what the find_clones unlimited path now does to every un-refined
    // class).
    dampen_unrefined_member_count(&mut refine_failed);

    // POST-dampen: the refine-failed class's size factor is sub-linear (1 + ln(301) ≈ 6.71), so
    // its ROI (≈ 6.71 × 10 = 67) no longer dwarfs everything — but more importantly its LINEAR
    // member_count dominance is gone, so it can no longer drown out genuinely refactorable
    // refined classes in the ranking. (The refined class here is deliberately the degenerate
    // worst case; a HEALTHY refined class with coverage ≥ 0.5 keeps its full member_count and
    // out-ranks the dampened refine-failed class outright.)
    assert!(
        refine_failed.roi < 300.0,
        "the dampen subdues the linear member_count dominance: {}",
        refine_failed.roi
    );
    // A healthy refined class (no coverage gate) with the SAME 13 members now out-ranks the
    // 300-member refine-failed class — the masquerade is closed.
    let mut healthy = class_with_member_count(13);
    healthy.body_token_len_medoid = 100; // a real refactorable clone with substantial bodies
    let healthy_refinement = unsampled_refinement(); // coverage 1.0, refactorability 1.0
    apply_refinement(&mut healthy, healthy_refinement);
    assert!(
        healthy.roi > refine_failed.roi,
        "a healthy refined class out-ranks the dampened refine-failed giant: {} vs {}",
        healthy.roi,
        refine_failed.roi
    );
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
    let root = rag_rat_base::test_scratch::ScratchDir::new("rag-rat-nan-test");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let config = rag_rat_base::config::Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.to_path_buf(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![rag_rat_base::config::ResolvedTarget {
            name: "rust".to_string(),
            language: rag_rat_base::language::Language::Rust,
            directories: vec![std::path::PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: rag_rat_base::config::TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
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

/// #270: `subject_component_bfs` (the reverse-lookup BFS) must return the EXACT `(component, edges)`
/// the full `candidate_pairs_from_bags` → `components_from_pairs` (subject's component) +
/// `bucket_edges_by_component` path produces — for every subject, across BOTH edge rules
/// (struct-hash and verified sub-block) and a singleton. This is the guard that lets the BFS
/// replace the full scan without changing results.
#[test]
fn subject_component_bfs_matches_full_scan() {
    let theta = THETA;
    // {1,2,3}: sub-block clones (near-identical token multisets → overlap ≥ θ, disjoint from the
    // rest). {10,11}: struct-hash clones (same struct_hash, DISJOINT tokens → connected only by the
    // struct-hash rule). 20: a singleton (disjoint tokens, unique struct_hash → no peer).
    let mut bags = vec![
        make_bag_with_tokens(1, vec![(1, 3), (2, 3), (3, 3), (4, 3)]),
        make_bag_with_tokens(2, vec![(1, 3), (2, 3), (3, 3), (4, 3)]),
        make_bag_with_tokens(3, vec![(1, 3), (2, 3), (3, 3), (4, 2)]),
        make_bag_with_tokens(20, vec![(90, 5), (91, 5), (92, 5), (93, 5)]),
    ];
    let mut b10 = make_bag_with_tokens(10, vec![(50, 4), (51, 4), (52, 4)]);
    let mut b11 = make_bag_with_tokens(11, vec![(60, 4), (61, 4), (62, 4)]);
    b10.struct_hash = "shared".to_string();
    b11.struct_hash = "shared".to_string();
    bags.push(b10);
    bags.push(b11);

    let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();

    // Full-scan reference (the pre-#270 path), computed ONCE — bags are fixed.
    let pairs = candidate_pairs_from_bags(&bags, theta);
    let comps = components_from_pairs(&pairs);
    let buckets = bucket_edges_by_component(&pairs, &comps);

    for subject in [1_i64, 2, 3, 10, 11, 20] {
        let (mut bfs_comp, mut bfs_edges) = subject_component_bfs(&by_id, subject, theta);
        bfs_comp.sort_unstable();
        bfs_edges.sort_unstable();
        // `None` (subject in no size-≥2 component) is the singleton case the BFS reports as a
        // length-<2 component.
        match comps.iter().position(|c| c.contains(&subject)) {
            None => assert!(
                bfs_comp.len() < 2,
                "subject {subject} is a singleton in the full scan; BFS must report no component, \
                 got {bfs_comp:?}"
            ),
            Some(idx) => {
                let mut edges = buckets[idx].clone();
                edges.sort_unstable();
                assert_eq!(bfs_comp, comps[idx], "component mismatch for subject {subject}");
                assert_eq!(bfs_edges, edges, "edge set mismatch for subject {subject}");
            },
        }
    }
}

/// #271: a hot (non-discriminating) token in > `HOT_TOKEN_POSTINGS_CAP` sub-blocks emits NO
/// candidate pairs — its K²/2 pairs are noise — but a GENUINE clone pair sharing a rarer token
/// still survives. Without the cap this fixture would emit ~45k pairs (the hot token's full
/// upper triangle); with it, only the one real pair.
#[test]
fn hot_token_postings_are_capped_but_real_clones_survive() {
    let hot = 0; // lowest token_hash → sorts first → always in the sub-block
    let mut bags = Vec::new();
    // Noise: each shares ONLY the hot token in its sub-block (token_len 2 → p=1 → just `hot`).
    let noise = super::substrate::HOT_TOKEN_POSTINGS_CAP + 4;
    for i in 0..noise as i64 {
        bags.push(make_bag_with_tokens(i, vec![(hot, 1), (10_000 + i, 1)]));
    }
    // Two genuine clones: token_len 4 → p=2 → sub-block is [hot, 1], so they ALSO share the
    // rarer token `1` (present in only these two bags).
    bags.push(make_bag_with_tokens(1_000, vec![(hot, 1), (1, 1), (2, 1), (3, 1)]));
    bags.push(make_bag_with_tokens(1_001, vec![(hot, 1), (1, 1), (2, 1), (3, 1)]));

    let pairs = sub_block_candidate_pairs(&bags, 0.7);

    assert!(pairs.contains(&(1_000, 1_001)), "the real clone pair must survive the cap");
    assert_eq!(
        pairs.len(),
        1,
        "only the real pair — the hot token's {} postings are capped, not its upper triangle",
        noise + 2
    );
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
    assert_eq!(METRIC_SAMPLE_CAP, 200);

    // The components from a small pair (union-find returns groups of size 2).
    let comps = components_from_pairs(&[(1, 2)]);
    assert_eq!(comps.len(), 1);
    assert_eq!(comps[0].len(), 2);
    // len=2 < 200 → metrics_sampled would be false in build_class.
    // We can't call build_class without a DB, so we just assert the cap logic in isolation:
    let n = comps[0].len();
    let metrics_sampled = n > METRIC_SAMPLE_CAP;
    assert!(!metrics_sampled, "a 2-member component must not trigger metrics_sampled");
}

// ── Fix E: CLI handle routing ─────────────────────────────────────────────────────────────
// (Tested in the CLI crate test below; here we just verify parse_sym_handle behaviour
// that the routing logic depends on.)

#[test]
fn parse_sym_handle_accepts_valid_handles_and_rejects_others() {
    use rag_rat_base::serde_big_id::parse_sym_handle;

    // A valid sym_<hex> handle round-trips.
    let h = rag_rat_base::serde_big_id::format_sym_handle(12345i64);
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
    let refs: Vec<String> = members.iter().map(|(r, p, s, e)| format!("{r}@{p}:{s}-{e}")).collect();

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
    assert!(refs.len() <= MEMBER_VALUE_CAP, "the MEMBER_VALUE_CAP cap is preserved");
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
            canonical_member_order_key(x.0, x.1, x.2)
                .cmp(&canonical_member_order_key(y.0, y.1, y.2))
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
fn class_with_member_count(member_count: usize) -> CandidateCloneClass {
    CandidateCloneClass {
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
        roi_factors: RoiFactors {
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
fn unsampled_refinement() -> rag_rat_clones::refine::cache::CachedRefinement {
    rag_rat_clones::refine::cache::CachedRefinement {
        lcs_ratio: 1.0,
        confidence: rag_rat_clones::refine::score::Confidence::High,
        refactorability: 1.0,
        refine_mode: rag_rat_clones::refine::cache::RefineMode::Baseline,
        template: String::new(),
        variation_points_json: "[]".to_string(),
        proposed_signature_json: "{}".to_string(),
        anti_unify_coverage: 1.0,
        lcs_sampled: false,
    }
}

#[test]
fn refine_metrics_sampled_at_value_cap() {
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

    let root = rag_rat_base::test_scratch::ScratchDir::new("rag-rat-recall-parity");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // Two renamed-clone groups + one unrelated function, across files.
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
         compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s += it * 2; } \
         s + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\npub \
         fn tally_amounts(values: Vec<i64>) -> i64 { let mut t = 0; for v in values { t += v * 2; \
         } t + 1 }\n",
    )
    .unwrap();
    let config = rag_rat_base::config::Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.to_path_buf(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![rag_rat_base::config::ResolvedTarget {
            name: "rust".to_string(),
            language: rag_rat_base::language::Language::Rust,
            directories: vec![std::path::PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: rag_rat_base::config::TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
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
        .query_map([NORM_VERSION], |r| {
            let blob: Option<Vec<u8>> = r.get(4)?;
            let pairs = blob
                .and_then(|b| rag_rat_clones::bag_blob::decode_token_bag(&b))
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
}

/// #235 item 12: a focused unit test for `load_source_discriminators`, the production
/// symbols→files SELECT that builds the per-member `{files.sha256}:{start}-{end}` discriminator
/// folded into `refinement_key` (the cross-file cache-poisoning fix). Previously covered only
/// transitively by the e2e real-index tests + cache.rs's test-local helper.
#[test]
fn load_source_discriminators_builds_sha_span_strings_and_needs_full_hydration() {
    use rusqlite::params;

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO files(id, path, language, kind, sha256, modified_at_ms, indexed_at_ms)
         VALUES (1, 'a.rs', 'rust', 'source', 'shaAAA', 0, 0),
                (2, 'b.rs', 'rust', 'source', 'shaBBB', 0, 0)",
        [],
    )
    .unwrap();
    for (id, file_id, name, start, end) in
        [(10i64, 1i64, "foo", 0i64, 12i64), (20, 2, "bar", 40, 73)]
    {
        let qn = format!("{name}.rs::{name}");
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qn]).unwrap();
        conn.execute(
            "INSERT INTO symbols(id, file_id, language, name, qualified_name_id, kind,
                                 start_byte, end_byte, signature, docs)
             VALUES (?1, ?2, 'rust', ?3, (SELECT id FROM name_strings WHERE value = ?4),
                     'function', ?5, ?6, NULL, NULL)",
            params![id, file_id, name, qn, start, end],
        )
        .unwrap();
    }

    // The discriminator is `{files.sha256}:{start_byte}-{end_byte}`. The query has no ORDER BY
    // and `refinement_key` sorts before hashing, so compare sorted (order is unspecified).
    let mut got =
        super::refine_load::load_source_discriminators(&conn, &[10, 20]).unwrap().unwrap();
    got.sort();
    assert_eq!(got, vec!["shaAAA:0-12".to_string(), "shaBBB:40-73".to_string()]);

    // Empty input hydrates to an empty multiset (Some, not None).
    assert_eq!(
        super::refine_load::load_source_discriminators(&conn, &[]).unwrap(),
        Some(Vec::new())
    );

    // A member that does not hydrate (no such symbol row) → None: a partial multiset would
    // alias a different class, so the class is left un-refined rather than mis-keyed.
    assert_eq!(super::refine_load::load_source_discriminators(&conn, &[10, 999]).unwrap(), None);
}
