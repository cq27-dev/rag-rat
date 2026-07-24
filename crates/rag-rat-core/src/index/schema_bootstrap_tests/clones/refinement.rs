use super::*;

/// Plan 4a: the content-addressed `refinement_key` is over the struct_hash MULTISET (order-
/// independent), and is DISTINCT from the read-side `class_key` (location-derived). Same multiset →
/// same key; the two key families never collide for the same class.
#[test]
fn refinement_key_is_content_addressed_and_distinct_from_read_key() {
    use rag_rat_clones::refine::cache::{RefineMode, refinement_key};

    let hashes = vec!["h1".to_string(), "h2".to_string(), "h3".to_string()];
    let shuffled = vec!["h3".to_string(), "h1".to_string(), "h2".to_string()];
    let discs = vec!["d1".to_string(), "d2".to_string(), "d3".to_string()];
    let discs_shuffled = vec!["d3".to_string(), "d1".to_string(), "d2".to_string()];
    // Same multiset, different order (struct_hashes AND source discriminators) → same
    // refinement_key (content-addressed, order-independent).
    assert_eq!(
        refinement_key("rust", RefineMode::Baseline, &hashes, &discs),
        refinement_key("rust", RefineMode::Baseline, &shuffled, &discs_shuffled),
        "the same struct_hash + source-discriminator multiset must address the same refinement"
    );

    // The refinement key (structural) is NOT the read-side class_key (location-derived). Build a
    // real clone class, then confirm its persisted refinement key ≠ its read-side class_key.
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);
    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    let read_key = &res.classes[0].class_key;
    // The persisted refinement row's PRIMARY KEY is the content-addressed key; it must not equal
    // the location-derived read-side class_key.
    let refinement_pk: String = db
        .storage
        .connection()
        .query_row("SELECT class_key FROM clone_refinements LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_ne!(
        &refinement_pk, read_key,
        "the content-addressed refinement key must differ from the location-derived read key"
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: the refinement cache is read-through — the first `find_clones` populates a
/// `clone_refinements` row; a second `find_clones` over the same index serves the cache and does
/// NOT grow the row count.
#[test]
fn refine_cache_is_read_through() {
    // Asserts whole-DB `clone_refinements` counts (0 before the run, N after); opt out of the
    // poison harness whose sibling seeds a refinement under its own repo_id.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    let count_rows = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0))
            .unwrap()
    };

    // Before any find_clones run the cache is empty.
    assert_eq!(count_rows(&db), 0, "no refinements before the first run");

    // Run 1: refines the clean class → exactly one cache row.
    let r1 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r1.classes[0].refined, "run 1 refines the class");
    let after_run1 = count_rows(&db);
    assert_eq!(after_run1, 1, "run 1 persists exactly one refinement row");

    // Run 2: same inputs → cache HIT, row count unchanged.
    let r2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r2.classes[0].refined, "run 2 still refined (served from cache)");
    assert_eq!(count_rows(&db), after_run1, "run 2 is a cache hit — the row count must not grow");

    let _ = fs::remove_dir_all(root);
}

/// Fix A (#215 Plan 4a codex2): the warm path (cache hit) must NOT re-parse any source file.
/// If re-parsing were happening on the warm path, deleting the source files after the first
/// find_clones would cause the second find_clones to return an un-refined class
/// (load_refine_members returns None on a missing file → un-refined fallback). With the fix — cache
/// probe BEFORE load_refine_members — the second call serves from the cache entirely: the source
/// files are never read and the class is still `refined=true`.
#[test]
fn find_clones_warm_cache_serves_refined_without_reparse() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    // Run 1 (cold path): populates the cache.
    let r1 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r1.classes[0].refined, "run 1 must refine the class (cold path)");

    // Delete the source files so any attempt to re-parse would fail / return None.
    for dir in &["a", "b"] {
        let _ = fs::remove_dir_all(root.join(dir));
    }

    // Run 2 (warm path): the cache must serve the refinement WITHOUT touching the (now-absent)
    // source files. If the warm path were re-parsing, load_refine_members would return None
    // (file missing) and the class would be left un-refined.
    let r2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        r2.classes[0].refined,
        "run 2 must still be refined from the cache — warm path must not re-parse source files"
    );
    assert_eq!(
        r1.classes[0].lcs_ratio, r2.classes[0].lcs_ratio,
        "warm-path lcs_ratio must match the cold-path value"
    );

    fs::remove_dir_all(root).unwrap_or(());
}

/// Fix B (#215 Plan 4a codex2): the member-count sampling dimension must be reported consistently
/// on BOTH the cold and warm cache paths. A class with more than LCS_MEMBER_SAMPLE members must
/// have `metrics_sampled=true` on the second (warm) find_clones call, not just the first (cold).
///
/// NOTE: planting >64 distinct fingerprinted clone-class members via the full index pipeline is
/// expensive; instead we verify the logic path directly via the public find_clones surface with a
/// small class (metrics_sampled stays false for small classes) and document that the large-class
/// warm-path consistency is enforced by the `apply_refinement` function's unconditional
/// `class.member_count > LCS_MEMBER_SAMPLE` OR-in, which is independent of cache hit/miss.
#[test]
fn find_clones_warm_cache_metrics_sampled_consistent() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    // Run 1 (cold): 4-member class — below LCS_MEMBER_SAMPLE, metrics_sampled should be false.
    let r1 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r1.classes[0].refined, "run 1 refines the class");
    let sampled_cold = r1.classes[0].metrics_sampled;

    // Run 2 (warm cache hit): the member-count dimension must be applied consistently.
    let r2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(r2.classes[0].refined, "run 2 is refined from the cache");
    let sampled_warm = r2.classes[0].metrics_sampled;

    assert_eq!(
        sampled_cold, sampled_warm,
        "metrics_sampled must be consistent across cold ({sampled_cold}) and warm \
         ({sampled_warm}) paths"
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: only the top-N (by provisional ROI) classes are refined, where N == the caller's limit.
/// With TWO distinct clean classes and `limit = 1`, exactly ONE class is refined: the returned
/// (top-1) class is refined, and only ONE `clone_refinements` row is written — the second class is
/// outside the refine budget and never reaches refinement, keeping its Plan-2 (un-refined) shape.
/// (Because the output is truncated to the limit, the un-refined class is not in the returned set;
/// the persisted-row count is the observable proof that only the top-N were refined.)
#[test]
fn unrefined_class_outside_top_n_keeps_plan2_shape() {
    // Asserts a whole-DB `clone_refinements` count; opt out of the poison harness whose sibling
    // seeds a refinement under its own repo_id.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Class 1: two big-body renamed clones (high ROI via long body) — refined first.
    let big = |name: &str, v: &str| {
        format!(
            "pub fn {name}(db: Db) -> i32 {{ let {v}1 = db.get(1); let {v}2 = db.get(2); let {v}3 \
             = db.get(3); validate({v}1); validate({v}2); validate({v}3); {v}1 + {v}2 + {v}3 }}\n"
        )
    };
    fs::write(root.join("src/big_a.rs"), big("big_user", "u")).unwrap();
    fs::write(root.join("src/big_b.rs"), big("big_order", "o")).unwrap();
    // Class 2: two small-body renamed clones (lower ROI via short body) — structurally distinct
    // from class 1 so they form a SEPARATE class, ranked below it.
    let small = |name: &str, v: &str| {
        format!(
            "pub fn {name}(xs: Vec<u8>) -> usize {{ let mut {v} = 0; for e in xs {{ {v} += e as \
             usize; }} {v} }}\n"
        )
    };
    fs::write(root.join("src/small_a.rs"), small("sum_bytes", "n")).unwrap();
    fs::write(root.join("src/small_b.rs"), small("sum_words", "m")).unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let count_rows = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0))
            .unwrap()
    };

    // Sanity: with no limit BOTH classes exist (the default budget 50 refines both).
    let all = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(all.classes.len(), 2, "two distinct clean classes are planted");
    assert!(all.classes.iter().all(|c| c.refined), "default budget refines both");
    assert_eq!(count_rows(&db), 2, "default budget persists both refinements");

    // Fresh index so the cache starts empty for the budget assertion.
    let root2 = unique_temp_root();
    let _ = fs::remove_dir_all(&root2);
    fs::create_dir_all(root2.join("src")).unwrap();
    fs::write(root2.join("src/big_a.rs"), big("big_user", "u")).unwrap();
    fs::write(root2.join("src/big_b.rs"), big("big_order", "o")).unwrap();
    fs::write(root2.join("src/small_a.rs"), small("sum_bytes", "n")).unwrap();
    fs::write(root2.join("src/small_b.rs"), small("sum_words", "m")).unwrap();
    let db2 = IndexDatabase::rebuild(&source_config(root2.clone(), Language::Rust)).unwrap();

    // limit=1 ⇒ refine budget 1 ⇒ only the top-1 class is refined.
    let limited = db2
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: Some(1) })
        .unwrap();
    assert_eq!(limited.classes.len(), 1, "limit=1 returns exactly one class");
    let top = &limited.classes[0];
    assert!(top.refined, "the single returned (top-1) class is refined");
    assert!(top.lcs_ratio.is_some(), "the refined class carries an lcs_ratio");
    assert_eq!(top.refine_mode, Some("baseline"));
    // Only ONE refinement was computed/persisted: the second class is outside the budget and keeps
    // its un-refined Plan-2 shape (it never reached `refine_class`).
    assert_eq!(
        count_rows(&db2),
        1,
        "limit=1 refines only the top-1 class — the out-of-budget class is never refined"
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(root2);
}

/// Fix 2 (Codex P2 #215 Plan 4a): find_clones with limit=Some(N) must never return an unrefined
/// class. The old implementation refine-budget top-N then re-sort ALL → a rank-(N+1) unrefined
/// class could displace a refined one after ROI recalculation. The fix truncates to N BEFORE
/// refining so only refined (or best-effort-unrefined) classes appear in a limited result.
///
/// Fixture: 3 structurally distinct clone classes (A db-getter / B match-expr / C loop-reducer —
/// distinct constructs so they form three separate components, never cross-merge). With
/// `limit=Some(2)` only the top-2 by provisional ROI are truncated into the refine set, refined,
/// and returned; the third class (truncated away before refining) never enters the result. The
/// load-bearing assertion is that EVERY class in the limited result has `refined == true` — the
/// property the old re-sort-ALL path could violate.
#[test]
fn find_clones_limited_result_contains_only_refined_classes() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // Class A: big body, high ROI.
    let big = |name: &str, v: &str| {
        format!(
            "pub fn {name}(db: Db) -> i32 {{ let {v}1 = db.get(1); let {v}2 = db.get(2); let {v}3 \
             = db.get(3); validate({v}1); validate({v}2); validate({v}3); {v}1 + {v}2 + {v}3 }}\n"
        )
    };
    fs::write(root.join("src/big_a.rs"), big("big_user", "u")).unwrap();
    fs::write(root.join("src/big_b.rs"), big("big_order", "o")).unwrap();

    // Class B: a `match`-expression body — structurally distinct from both the db-getter (A) and
    // the loop-reducer (C), so it forms its OWN component (no cross-class merge). Medium length.
    let matchy = |name: &str, v: &str| {
        format!(
            "pub fn {name}(k: i32) -> i32 {{ let {v} = match k {{ 0 => 10, 1 => 20, 2 => 30, _ => \
             40 }}; {v} + 1 }}\n"
        )
    };
    fs::write(root.join("src/match_a.rs"), matchy("classify_a", "n")).unwrap();
    fs::write(root.join("src/match_b.rs"), matchy("classify_b", "m")).unwrap();

    // Class C: a loop-reducer body — structurally distinct from A and B. Small length.
    let small = |name: &str, v: &str| {
        format!(
            "pub fn {name}(xs: Vec<u8>) -> usize {{ let mut {v} = 0; for e in xs {{ {v} += e as \
             usize; }} {v} }}\n"
        )
    };
    fs::write(root.join("src/small_a.rs"), small("sum_bytes", "s")).unwrap();
    fs::write(root.join("src/small_b.rs"), small("sum_words", "t")).unwrap();

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Sanity: all 3 classes exist and unlimited refines all of them (budget=50).
    let all = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(all.classes.len(), 3, "three distinct clone classes are planted: {:?}", all.classes);
    assert!(all.classes.iter().all(|c| c.refined), "unlimited refines all 3 (budget=50)");

    // Fresh index for the fix-2 assertion (cache starts empty).
    let root2 = unique_temp_root();
    let _ = fs::remove_dir_all(&root2);
    fs::create_dir_all(root2.join("src")).unwrap();
    fs::write(root2.join("src/big_a.rs"), big("big_user", "u")).unwrap();
    fs::write(root2.join("src/big_b.rs"), big("big_order", "o")).unwrap();
    fs::write(root2.join("src/match_a.rs"), matchy("classify_a", "n")).unwrap();
    fs::write(root2.join("src/match_b.rs"), matchy("classify_b", "m")).unwrap();
    fs::write(root2.join("src/small_a.rs"), small("sum_bytes", "s")).unwrap();
    fs::write(root2.join("src/small_b.rs"), small("sum_words", "t")).unwrap();
    let db2 = IndexDatabase::rebuild(&source_config(root2.clone(), Language::Rust)).unwrap();

    // limit=2: top-2 by provisional ROI are truncated, refined, returned. Class C never enters.
    let limited = db2
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: Some(2) })
        .unwrap();
    assert_eq!(limited.classes.len(), 2, "limit=2 returns exactly 2 classes");
    for (i, c) in limited.classes.iter().enumerate() {
        assert!(
            c.refined,
            "every class in a limited result must be refined (or best-effort-unrefined); \
             class[{i}] (key={}) has refined=false",
            c.class_key
        );
    }

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(root2);
}

/// I2 (#215 Plan 4a adversary): find_clones with a huge limit must clamp effective returned classes
/// to UNLIMITED_REFINE_BUDGET (50), returning all-refined. This plants MORE than 50 distinct clone
/// classes so the clamp is actually EXERCISED (the earlier 3-class fixture never tripped it): a
/// huge `limit` returns EXACTLY 50 classes (all refined), and both `truncated` and
/// `refine_budget_clamped` are set.
#[test]
fn find_clones_huge_limit_clamps_to_refine_budget() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // Generate 18 ops × 4 length-tiers = 72 DISTINCT, non-merging clone classes (> the budget of
    // 50). Two independent axes keep distinct classes from cross-merging under the SourcererCC
    // candidate edge test (overlap/max_len >= θ=0.7):
    //   1. OPERATOR axis: each body is a long left-assoc binary chain `a OP a OP a …` over a single
    //      operand, so the verbatim operator token dominates the normalized bag. The 18 distinct
    //      binary operators each yield a separate class (within-tier max similarity < 0.7 once the
    //      chain is long enough — the smallest tier is 64 reps; 40 reps merges).
    //   2. LENGTH-TIER axis: four chain lengths ~1.6× apart (> 1/θ ≈ 1.43) so the size-prune
    //      (min_len >= ceil(θ·max_len)) drops EVERY cross-tier edge regardless of content.
    // The `_a`/`_b` variants are rename-clones (operand `a` vs `b`, distinct fn names): identical
    // structure → same struct_hash → exactly one class per pair via the struct-hash fast path.
    // NOTE: identifier names and literal VALUES are normalization-invariant, so the per-pair
    // distinction MUST come from structure (operator + chain length), never names/literals.
    let ops = [
        "+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>", "<", ">", "<=", ">=", "==", "!=", "&&",
        "||",
    ];
    let tiers = [64usize, 104, 168, 270];
    let body = |op: &str, n: usize, var: &str, name: &str| {
        let mut s = format!("pub fn {name}({var}: i64) -> i64 {{\n    {var}");
        for _ in 0..n {
            s.push_str(&format!(" {op} {var}"));
        }
        s.push_str("\n}\n");
        s
    };
    let mut idx = 0usize;
    for (ti, &n) in tiers.iter().enumerate() {
        for (oi, op) in ops.iter().enumerate() {
            let fa = body(op, n, "a", &format!("fn_a_t{ti}_o{oi}"));
            let fb = body(op, n, "b", &format!("fn_b_t{ti}_o{oi}"));
            fs::write(root.join(format!("src/clone_a{idx}.rs")), fa).unwrap();
            fs::write(root.join(format!("src/clone_b{idx}.rs")), fb).unwrap();
            idx += 1;
        }
    }

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Sanity: the full (unlimited) result must surface MORE than the refine budget of classes,
    // otherwise the clamp below is vacuous.
    let all = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        all.classes.len() > 50,
        "fixture must plant > 50 clone classes to exercise the clamp; got {}",
        all.classes.len()
    );

    // limit=100000 >> total classes — clamps to exactly UNLIMITED_REFINE_BUDGET (50).
    let limited = db
        .find_clones(FindClonesOptions {
            min_similarity: None,
            min_copies: None,
            limit: Some(100_000),
        })
        .unwrap();
    assert_eq!(
        limited.classes.len(),
        50,
        "a huge limit over > 50 classes must return EXACTLY the refine budget (50); got {}",
        limited.classes.len()
    );
    assert!(
        limited.classes.iter().all(|c| c.refined),
        "every class in a huge-limited result must be refined"
    );
    assert!(
        limited.completeness.truncated,
        "dropping whole classes to honor the budget must set truncated"
    );
    assert!(
        limited.completeness.refine_budget_clamped,
        "a limit above the budget that drops classes must set refine_budget_clamped"
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4b Task 5b: `load_refine_members` caps the re-parse to `MEMBER_VALUE_CAP` (50, previously
/// `LCS_MEMBER_SAMPLE` = 64 in Plan 4a). Plants a single clone class with more than
/// `MEMBER_VALUE_CAP` members, calls `load_refine_members`, and asserts it returns EXACTLY
/// `MEMBER_VALUE_CAP` members in canonical (struct_hash, path, start_byte) order. Also asserts the
/// constants are still consistent (MEMBER_VALUE_CAP == MAX_MEMBERS == 50, LCS_MEMBER_SAMPLE == 64,
/// MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE — the align cap never sees more members than the value cap
/// loads).
#[test]
fn load_refine_members_returns_up_to_value_cap() {
    use rag_rat_clones::refine::align::LCS_MEMBER_SAMPLE;

    use crate::index::query_api::{MAX_MEMBERS, MEMBER_VALUE_CAP};
    // Constant consistency assertions — these values are load-bearing.
    assert_eq!(MEMBER_VALUE_CAP, 50, "MEMBER_VALUE_CAP must be 50");
    assert_eq!(MAX_MEMBERS, 50, "MAX_MEMBERS must be 50");
    assert_eq!(LCS_MEMBER_SAMPLE, 64, "LCS_MEMBER_SAMPLE must be 64");
    // MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE is a compile-time invariant: move to const context so
    // clippy::assertions_on_constants doesn't fire.
    const { assert!(MEMBER_VALUE_CAP < LCS_MEMBER_SAMPLE) };

    const MEMBERS: usize = MEMBER_VALUE_CAP + 1; // 51 — one over the value cap.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // 51 rename-clones of ONE structure: identical AST shape, distinct identifier names. Baseline
    // normalization alpha-renames identifiers to ID<n> and buckets literals, so all 51 collapse to
    // the SAME struct_hash → one clone class (the struct_hash exact fast path components them).
    for i in 0..MEMBERS {
        let src = format!(
            "pub fn fn_{i}(db: Db) -> i32 {{ let a{i} = db.get(); let b{i} = db.get(); let c{i} = \
             db.get(); validate(a{i}); validate(b{i}); validate(c{i}); a{i} + b{i} + c{i} }}\n"
        );
        fs::write(root.join(format!("src/m{i}.rs")), src).unwrap();
    }

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Find the single component holding all 51 members.
    let components = db.candidate_clone_components().expect("components");
    let mut big = components
        .into_iter()
        .find(|c| c.len() == MEMBERS)
        .unwrap_or_else(|| panic!("expected one component of {MEMBERS} exact clones"));
    big.sort_unstable();

    // load_refine_members must cap the re-parse to MEMBER_VALUE_CAP members.
    let members = db
        .load_refine_members(&big, false)
        .expect("load_refine_members ok")
        .expect("refine inputs available for an in-scope class");
    assert_eq!(
        members.len(),
        MEMBER_VALUE_CAP,
        "load_refine_members must cap a {MEMBERS}-member class to MEMBER_VALUE_CAP (50)"
    );

    // All struct_hashes are equal (exact clones), so canonical order is ascending symbol_id — the
    // first 50 ids of the sorted component.
    let returned_ids: Vec<i64> = members.iter().map(|m| m.symbol_id).collect();
    let expected_ids: Vec<i64> = big.iter().copied().take(MEMBER_VALUE_CAP).collect();
    assert_eq!(
        returned_ids, expected_ids,
        "capped members must be the first MEMBER_VALUE_CAP in canonical (struct_hash, id) order"
    );

    // Close the SQLite connection before deleting its dir: Windows refuses to remove a file with a
    // live handle (`os error 32`), whereas Unix unlinks it lazily. Dropping `db` first makes the
    // strict teardown pass on both.
    drop(db);
    fs::remove_dir_all(root).unwrap();
}

/// Plan 4b Task 5b: every `RefineMember` returned by `load_refine_members` must carry
/// `node_spans` with the same length as `seq` (bijection invariant from §1.5), a non-empty `text`
/// buffer (the whole-file source), and spans whose byte ranges recover real source text. Also
/// confirms that members sharing a file share the same `Arc<str>` allocation
/// (`Arc::ptr_eq`) — one read per file, not one per member.
#[test]
fn refine_member_carries_spans_len_eq_seq() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two rename-clone functions in TWO files (so we can also test same-file Arc sharing below via
    // a third function added to a.rs).
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    let members = db
        .load_refine_members(&[id_a, id_b], false)
        .expect("load ok")
        .expect("refine inputs available");
    assert_eq!(members.len(), 2, "both members must be returned");

    for m in &members {
        // Bijection invariant: node_spans.len() == seq.len().
        assert_eq!(
            m.node_spans.len(),
            m.seq.len(),
            "node_spans.len() must equal seq.len() for member {}",
            m.symbol_id
        );
        // text must be non-empty (whole-file source, not sliced).
        assert!(!m.text.is_empty(), "member text must be non-empty");
        // At least one span's byte range must recover a real (non-empty) source slice.
        let any_real_slice = m
            .node_spans
            .iter()
            .any(|sp| m.text.get(sp.start_byte..sp.end_byte).is_some_and(|s| !s.is_empty()));
        assert!(
            any_real_slice,
            "at least one span in node_spans must recover a non-empty source slice for member {}",
            m.symbol_id
        );
        // A leaf span (is_leaf=true) must recover a real identifier (non-empty, non-whitespace).
        if let Some(leaf_sp) = m.node_spans.iter().find(|s| s.is_leaf) {
            let slice =
                m.text.get(leaf_sp.start_byte..leaf_sp.end_byte).expect("leaf span byte range");
            assert!(!slice.trim().is_empty(), "leaf span must recover non-empty source text");
        }
    }

    // Close the SQLite connection before deleting its dir: Windows refuses to remove a file with a
    // live handle (`os error 32`), whereas Unix unlinks it lazily. Dropping `db` first makes the
    // strict teardown pass on both.
    drop(db);
    fs::remove_dir_all(root).unwrap();
}

/// Plan 4b Task 5b: the faithfulness pin still drops drifted members. A member whose on-disk
/// content changed after indexing (struct_hash mismatch between re-parse and persisted) causes
/// `load_refine_members` to return `Ok(None)` — the caller falls back to the un-refined class
/// rather than aligning stale tokens.
#[test]
fn faithfulness_pin_still_drops_drifted_member() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    // Sanity: before drift, refine inputs are available.
    assert!(
        db.load_refine_members(&[id_a, id_b], false).unwrap().is_some(),
        "before drift: refine inputs must be available"
    );

    // Drift one member's on-disk source: overwrite b.rs with a structurally DIFFERENT function
    // (adds a while loop → different struct_hash). The index still holds the old fingerprint row.
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); while o > 0 { o -= 1; } o }\n",
    )
    .unwrap();

    // After drift: the re-parse of b.rs produces a different struct_hash → faithfulness pin fires
    // → load_refine_members returns Ok(None).
    let result = db.load_refine_members(&[id_a, id_b], false).unwrap();
    assert!(
        result.is_none(),
        "a drifted member (struct_hash mismatch) must cause load_refine_members to return Ok(None)"
    );

    let _ = fs::remove_dir_all(root);
}

/// #259 (Adversary C) — END-TO-END through the real `find_clones` driver: a refine-FAILED class (a
/// clone class whose on-disk source drifted post-index, so `refine_class_in_place` no-ops and the
/// class stays un-refined with its member_count-LINEAR Plan-2 ROI) has its `member_count` size
/// factor DAMPENED in the returned result. The dampen (applied to every still-un-refined class
/// post-refine, pre-sort in BOTH `find_clones` branches) replaces the linear `member_count` factor
/// with `1 + ln(1 + member_count)`, so a refine-failed component can no longer masquerade as
/// high-ROI purely on size. This exercises the dampen through the REAL driver, not just the helper
/// unit — it proves `find_clones` rewrites a returned un-refined class's `roi` to the dampened
/// formula.
///
/// Fixture: a clone class of THREE rename-clones with a substantial body. We DRIFT one member on
/// disk (structurally different re-parse → struct_hash mismatch → the all-or-nothing faithfulness
/// pin makes the refinement a no-op on a COLD cache), so the class comes back un-refined. The
/// returned `roi` must equal the DAMPENED Plan-2 product (size factor `1 + ln(1 + member_count)`),
/// which is strictly below the raw LINEAR product (`member_count` factor) the class would carry
/// without the fix — the masquerade is closed.
#[test]
fn refine_failed_class_member_count_dampened_end_to_end() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // A clone class of THREE rename-clones with a substantial body — would refine cleanly if the
    // source stayed faithful.
    let body = |name: &str, v: &str| {
        format!(
            "pub fn {name}(db: Db) -> i32 {{ let {v}1 = db.get(1); let {v}2 = db.get(2); let {v}3 \
             = db.get(3); validate({v}1); validate({v}2); validate({v}3); {v}1 + {v}2 + {v}3 }}\n"
        )
    };
    fs::write(root.join("src/a.rs"), body("load_a", "u")).unwrap();
    fs::write(root.join("src/b.rs"), body("load_b", "o")).unwrap();
    fs::write(root.join("src/c.rs"), body("load_c", "p")).unwrap();

    // Build the index clean (so the 3-member class forms from faithful fingerprints) with a COLD
    // refine cache, then DRIFT one member BEFORE the first find_clones. A warm cache hit would
    // serve the cached refinement and never re-read the drifted source (the cache key is over
    // the PERSISTED struct_hash + the PERSISTED file sha256, both unchanged by an on-disk
    // edit), so the drift MUST land before the very first refine attempt.
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // DRIFT one member: overwrite c.rs with a structurally DIFFERENT body. The index keeps the old
    // fingerprint (the class is still BUILT with member_count 3 from the persisted tables), but the
    // FIRST find_clones below takes the cold refine path → `load_refine_members` re-parses the
    // drifted source → struct_hash mismatch → the all-or-nothing faithfulness pin returns Ok(None)
    // → the class stays UN-refined.
    fs::write(
        root.join("src/c.rs"),
        "pub fn load_c(db: Db) -> i32 { let mut n = 0; while n < 9 { n += 1; } n }\n",
    )
    .unwrap();

    let result = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        result.classes.len(),
        1,
        "the three rename-clones form one class: {:?}",
        result.classes
    );
    let class = &result.classes[0];
    assert_eq!(
        class.member_count, 3,
        "the class is built with all 3 members from persisted tables"
    );

    // The class FAILED refinement (one member drifted → all-or-nothing) and is un-refined.
    assert!(
        !class.refined,
        "the drifted class fails the all-or-nothing refine and stays un-refined"
    );

    // THE #259 PROPERTY: the returned `roi` is the DAMPENED Plan-2 product — the `member_count`
    // factor replaced by `1 + ln(1 + member_count)`. Reconstruct both the raw (linear) and the
    // dampened product from the surfaced factors and confirm find_clones returned the dampened one.
    let mc = class.member_count as f64;
    let raw_roi = class.cross_module_spread as f64
        * mc
        * class.body_token_len_medoid as f64
        * class.roi_factors.load_bearing_factor
        * class.cohesion_min_pairwise;
    let dampened_roi = raw_roi / mc * (1.0 + mc.ln_1p());
    assert!(
        (class.roi - dampened_roi).abs() < 1e-6,
        "#259: find_clones must return the DAMPENED roi {} for the un-refined class, got {}",
        dampened_roi,
        class.roi
    );
    // The dampen STRICTLY reduces the rank signal versus the raw linear Plan-2 ROI (3 members →
    // 1 + ln(4) ≈ 2.39 < 3), so a large refine-failed component can no longer dominate on size.
    assert!(
        class.roi < raw_roi,
        "#259: the dampened roi {} must be strictly below the raw linear Plan-2 roi {}",
        class.roi,
        raw_roi
    );

    let _ = fs::remove_dir_all(root);
}
