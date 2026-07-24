use super::*;

/// `find_clones` integration test: four near-identical rename-clone functions across two
/// directories form one candidate class; metrics are plausible and completeness block is populated.
#[test]
fn find_clones_ranks_a_clean_clone_class_with_metrics() {
    use rag_rat_clones::NORM_VERSION;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();

    // Four rename-clone variants — identical structure, only the variable name changes.
    for (dir, name, var) in [
        ("a", "load_user", "u"),
        ("a", "load_order", "o"),
        ("b", "load_item", "i"),
        ("b", "load_blob", "x"),
    ] {
        fs::write(
            root.join(dir).join(format!("{name}.rs")),
            format!(
                "pub fn {name}(db: Db) -> i32 {{ let {var} = db.get(1); validate({var}); {var} + \
                 1 }}\n"
            ),
        )
        .unwrap();
    }
    // A structurally distinct function that must NOT join the clone class.
    fs::write(
        root.join("a/misc.rs"),
        "pub fn misc(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n += b as usize; } n }\n",
    )
    .unwrap();

    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("a"), PathBuf::from("b")],
            include: vec!["a/".to_string(), "b/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
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
    let db = IndexDatabase::rebuild(&config).unwrap();

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();

    assert_eq!(res.classes.len(), 1, "exactly one clone class (the four rename-clones)");
    let c = &res.classes[0];
    assert_eq!(c.member_count, 4, "all four rename-clone functions are members");
    // Plan 4a: a clean class inside the refine budget is REFINED (it was the only class, so the
    // top-N driver refined it). The class_kind flips to "refined_class" and `refined` is true.
    assert_eq!(c.class_kind, "refined_class");
    assert!(c.refined, "a clean class inside the refine budget is refined (Plan 4a)");
    assert_eq!(c.refine_mode, Some("baseline"), "refined classes carry the baseline refine mode");
    assert!(
        c.similarity_min > 0.9,
        "rename-clones are near-identical; expected similarity_min > 0.9, got {}",
        c.similarity_min
    );
    assert_eq!(c.cross_module_spread, 2, "members span two directories (a/ and b/)");
    assert_eq!(c.language, "rust");
    assert!(!c.class_key.is_empty());

    // Completeness block.
    assert_eq!(res.completeness.candidate_metric, "overlap_max_denominator");
    assert_eq!(res.completeness.normalizer_version, NORM_VERSION);
    assert!(!res.completeness.truncated);

    let _ = fs::remove_dir_all(root);
}

/// `clones_for_symbol` integration test: two rename-clone functions (a.rs / b.rs) form one
/// candidate class; the `Ref` selector resolves to that class, the `PathLine` selector at line 1
/// resolves to the same `class_key`, and a structurally distinct solo function → `None`.
#[test]
fn clones_for_symbol_returns_the_class_by_ref_and_by_path_line() {
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

    // --- Ref selector ---
    let by_ref_res =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    let by_ref = by_ref_res.class.as_ref().expect("src/a.rs::load_user should be in a clone class");
    assert_eq!(by_ref.member_count, 2, "class must contain both rename-clones");
    assert!(
        by_ref.members.iter().any(|m| m.r#ref.ends_with("b.rs::load_order")),
        "siblings must include the other clone; got: {:?}",
        by_ref.members.iter().map(|m| &m.r#ref).collect::<Vec<_>>()
    );

    // --- PathLine selector — same class_key as Ref ---
    let by_line_res = db
        .clones_for_symbol(CloneSymbolSelector::PathLine { path: "src/a.rs".into(), line: 1 })
        .unwrap();
    let by_line = by_line_res
        .class
        .as_ref()
        .expect("PathLine at line 1 in src/a.rs should resolve to the same clone class");
    assert_eq!(
        by_line.class_key, by_ref.class_key,
        "PathLine and Ref must resolve to the same class_key"
    );

    // --- Unrelated solo function → class: None ---
    // A structurally distinct function whose token bag won't reach θ=0.7 against the clones.
    fs::write(
        root.join("src/c.rs"),
        "pub fn solo(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n ^= b as usize; } n }\n",
    )
    .unwrap();
    let db2 = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let solo_res =
        db2.clones_for_symbol(CloneSymbolSelector::Ref("src/c.rs::solo".into())).unwrap();
    assert!(solo_res.class.is_none(), "a symbol in no clone class must have class: None");
    assert!(solo_res.symbol_resolved, "the solo symbol still resolves");
    assert!(solo_res.symbol_fingerprinted, "the solo function is eligible (fingerprinted)");

    // Post-condition: the clone rebuild/precompute must not touch a sibling repo (round-6 harness).
    crate::index::poison_sibling::assert_sibling_intact(db2.storage.connection());
    let _ = fs::remove_dir_all(root);
}

/// Fix 1 (#215): `min_similarity` is honored ALL the way through candidate generation, not merely
/// post-filtered. A borderline pair whose overlap/max ≈ 0.58 (in [0.5, 0.7)) is below the const θ
/// so it never even becomes a candidate at the default threshold — only a caller-supplied θ ≤ 0.58
/// widens candidate generation enough to surface it. The completeness block reports the θ used.
#[test]
fn find_clones_min_similarity_below_theta_widens_and_is_reported() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // `a` is a let-chain; `b` shares `a`'s first four statements then diverges into a loop+match,
    // so their token bags overlap moderately. Measured: token_lens 92 / 136, overlap/max ≈ 0.58.
    fs::write(
        root.join("src/a.rs"),
        "pub fn a(x: i32, y: i32) -> i32 { let p = alpha(x); let q = beta(y); let r = gamma(p); \
         let s = delta(q); let t = epsilon(r, s); p + q + r + s + t }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn b(x: i32, y: i32) -> i32 { let p = alpha(x); let q = beta(y); let r = gamma(p); \
         let s = delta(q); for item in items.iter() { let v = process(item); match v { 0 => total \
         += 1, _ => total += v } } if total > 0 { total } else { -1 } }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // θ = 0.5 (below the pair's ≈0.58 similarity): the pair becomes a candidate and is returned,
    // and the completeness block records the requested θ.
    let widened = db
        .find_clones(FindClonesOptions { min_similarity: Some(0.5), min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        widened.classes.len(),
        1,
        "θ=0.5 must surface the borderline pair as a class: {:?}",
        widened.classes
    );
    let sim = widened.classes[0].similarity_min;
    assert!(
        (0.5..0.7).contains(&sim),
        "the planted pair's similarity must sit in [0.5, 0.7): got {sim}"
    );
    assert_eq!(
        widened.completeness.min_similarity, 0.5,
        "completeness must report the θ actually used (0.5)"
    );

    // Default θ (None ⇒ 0.7): the pair is below threshold and must NOT be a candidate — proving
    // the widening was real (candidate generation, not just a post-filter relax).
    let default = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        default.classes.is_empty(),
        "θ=0.7 must NOT surface the borderline pair: {:?}",
        default.classes
    );
    assert_eq!(default.completeness.min_similarity, 0.7, "default completeness θ is 0.7");

    let _ = fs::remove_dir_all(root);
}

/// `min_similarity` is a similarity ratio θ = overlap/max_len and must lie in [0.5, 1.0]. Values
/// outside that range are rejected up front (before candidate generation) so a unit error (e.g. a
/// percentage like 1.5), a degenerate 0.0 floor, or any value below the 0.5 safety floor can't
/// cause O(S²) candidate-pair explosion in the inverted index.
#[test]
fn find_clones_rejects_out_of_range_min_similarity() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // A single clone pair so the index isn't empty; the range check fires regardless of contents.
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

    // 0.0 (below floor) → error; message must mention the valid range [0.5, 1.0].
    let zero = db.find_clones(FindClonesOptions {
        min_similarity: Some(0.0),
        min_copies: None,
        limit: None,
    });
    let err = zero.expect_err("min_similarity = 0.0 must be rejected").to_string();
    assert!(err.contains("[0.5, 1.0]"), "expected '[0.5, 1.0]' in error, got: {err}");

    // 0.4 (below floor) → also rejected.
    let below_floor = db.find_clones(FindClonesOptions {
        min_similarity: Some(0.4),
        min_copies: None,
        limit: None,
    });
    let err = below_floor
        .expect_err("min_similarity = 0.4 must be rejected (below 0.5 floor)")
        .to_string();
    assert!(err.contains("[0.5, 1.0]"), "expected '[0.5, 1.0]' in error for 0.4, got: {err}");

    // 1.5 (above 1.0) → error.
    let high = db.find_clones(FindClonesOptions {
        min_similarity: Some(1.5),
        min_copies: None,
        limit: None,
    });
    let err = high.expect_err("min_similarity = 1.5 must be rejected").to_string();
    assert!(err.contains("[0.5, 1.0]"), "expected '[0.5, 1.0]' in error for 1.5, got: {err}");

    // 1.0 (boundary, inclusive upper) → accepted.
    db.find_clones(FindClonesOptions { min_similarity: Some(1.0), min_copies: None, limit: None })
        .expect("min_similarity = 1.0 is the inclusive upper bound and must be accepted");

    // 0.5 (inclusive lower bound) → accepted.
    db.find_clones(FindClonesOptions { min_similarity: Some(0.5), min_copies: None, limit: None })
        .expect("min_similarity = 0.5 is the inclusive lower bound and must be accepted");

    let _ = fs::remove_dir_all(root);
}

/// Fix 2 (#215): `completeness.truncated` reflects whole CLASSES dropped by `limit`, not only
/// members capped within a class. Plant two distinct clone classes, ask for `limit=1`, and assert
/// the dropped second class flips `truncated`.
#[test]
fn find_clones_truncated_reflects_class_limit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Class 1: two rename-clones of a `load_*` accessor.
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
    // Class 2: two rename-clones of a structurally DIFFERENT `sum_*` reducer — its own component.
    fs::write(
        root.join("src/c.rs"),
        "pub fn sum_bytes(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n += b as usize; } n \
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/d.rs"),
        "pub fn sum_words(w: Vec<u8>) -> usize { let mut m = 0; for c in w { m += c as usize; } m \
         }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Sanity: with no limit there are two distinct classes and nothing is truncated.
    let all = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(all.classes.len(), 2, "two distinct clone classes are planted: {:?}", all.classes);
    assert!(!all.completeness.truncated, "no limit ⇒ not truncated");

    // limit=1 drops one whole class ⇒ truncated must be true (Fix 2).
    let limited = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: Some(1) })
        .unwrap();
    assert_eq!(limited.classes.len(), 1, "limit=1 returns exactly one class");
    assert!(
        limited.completeness.truncated,
        "dropping a whole class via the limit must set completeness.truncated"
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4 coherence-splits over-merged components; the A~B~C chain becomes coherent sub-classes.
///
/// A TRANSITIVE-chain component (A–B and B–C both ≥ θ, but A–C < θ) is over-merged by union-find
/// into one 3-member component. Plan 4a's `coherence_split` (greedy maximal clique cover) breaks
/// it: every returned class is internally coherent (all pairs ≥ θ), so NO single class contains all
/// three. For this fixture the cover yields BOTH coherent pairs — {A,B} and {B,C} — with B in both
/// (the overlap is correct: B coheres with two peers that are themselves incompatible).
/// `find_clones` therefore returns two 2-member classes, never the 3-member chain.
///
/// `clones_for_symbol(A)` returns the largest coherent group containing A — here the {A,B} pair.
/// `clones_for_symbol(C)` returns the coherent {B,C} pair (C is NO LONGER a singleton under the
/// clique cover), so a query ABOUT C surfaces a real refined sub-class, not the over-merged
/// fallback.
///
/// The fixture is empirically tuned and the test asserts the MEASURED edge similarities so it is
/// honest about the chain it plants (a tokenizer change that shifts the numbers reddens here, not
/// silently). At HEAD the measured edges are A/B≈0.74, B/C≈0.86, A/C≈0.67 — a genuine chain whose
/// weakest (A/C) endpoint sits below the default θ=0.70.
#[test]
fn coherence_split_applied_in_find_clones() {
    // The candidate metric is overlap/MAX_len, so the three members must be ~EQUAL length (a length
    // gap trips the size prune `min_len >= ceil(θ*max_len)` and kills the edges). Identifier names
    // alpha-rename to ID<n>, so only STRUCTURE drives the bag. Each member = a shared CORE of
    // let-bindings + TWO distinct structural slots built from DIFFERENT constructs (their tokens
    // don't overlap): A shares slot S1 with B; B shares slot S2 with C; A and C share neither, so
    // A/B and B/C clear θ while A/C falls below it.
    let core = "let c1 = ca(x); let c2 = cb(c1); let c3 = cc(c2);";
    let s1 = "if x > 0 { acc = p1(x); } else { acc = p2(x); } if acc > 1 { acc = p3(acc); } else \
              { acc = p4(acc); }";
    let s2 = "for it in xs { match it { 0 => acc += q1(it), 1 => acc += q2(it), _ => acc -= \
              q3(it) } } for jt in ys { match jt { 0 => acc += q4(jt), _ => acc -= q5(jt) } }";
    let sx = "while acc > 0 { acc = r1(acc); acc = r2(acc); acc = r3(acc); } while acc < 9 { acc \
              = r4(acc); acc = r5(acc); }";
    let sy = "loop { acc = s1f(acc); acc = s2f(acc); acc = s3f(acc); if acc == 0 { break; } } \
              loop { acc = s4f(acc); if acc < 0 { break; } }";
    // A = CORE + S1 + SX ; B = CORE + S1 + S2 ; C = CORE + S2 + SY.
    let a = format!("pub fn fa(x: i32) -> i32 {{ {core} {s1} {sx} 0 }}\n");
    let b = format!("pub fn fb(x: i32) -> i32 {{ {core} {s1} {s2} 0 }}\n");
    let c = format!("pub fn fc(x: i32) -> i32 {{ {core} {s2} {sy} 0 }}\n");
    let (a, b, c) = (a.as_str(), b.as_str(), c.as_str());

    const THETA: f64 = 0.7;

    // Measure each pairwise edge by rebuilding a two-file subset (so the only clone class is that
    // single pair, whose `similarity_min` IS the edge similarity). This makes the chain claim a
    // measured fact, not an assumption.
    let edge_sim = |src1: (&str, &str), src2: (&str, &str)| -> f64 {
        let r = unique_temp_root();
        let _ = fs::remove_dir_all(&r);
        fs::create_dir_all(r.join("src")).unwrap();
        fs::write(r.join(format!("src/{}.rs", src1.0)), src1.1).unwrap();
        fs::write(r.join(format!("src/{}.rs", src2.0)), src2.1).unwrap();
        let d = IndexDatabase::rebuild(&source_config(r.clone(), Language::Rust)).unwrap();
        // θ=0.5 (floor) so even a sub-default edge surfaces if ≥0.5; the class's similarity_min
        // is the pair's similarity. If no class forms at θ=0.5, the pair's similarity is < 0.5 —
        // which is still < θ=0.7 (THETA), satisfying the ac < THETA assertion. Return 0.0 as a
        // sentinel in that case.
        let res = d
            .find_clones(FindClonesOptions {
                min_similarity: Some(0.5),
                min_copies: None,
                limit: None,
            })
            .unwrap();
        let sim = res.classes.first().map(|c| c.similarity_min).unwrap_or(0.0);
        let _ = fs::remove_dir_all(r);
        sim
    };
    let ab = edge_sim(("a", a), ("b", b));
    let bc = edge_sim(("b", b), ("c", c));
    let ac = edge_sim(("a", a), ("c", c));
    assert!(ab >= THETA, "A/B must be a real (≥θ) edge: measured {ab}");
    assert!(bc >= THETA, "B/C must be a real (≥θ) edge: measured {bc}");
    assert!(
        ac < THETA,
        "A/C must be BELOW θ so the three only link transitively through B: measured {ac}"
    );

    // Now the full three-member scope. At the default θ=0.70 the over-merged union-find component
    // {A,B,C} is coherence-SPLIT: no returned class contains all three, and every returned class is
    // internally coherent (all pairs ≥ θ). The greedy clique cover yields BOTH coherent pairs:
    // {A,B} and {B,C}.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), a).unwrap();
    fs::write(root.join("src/b.rs"), b).unwrap();
    fs::write(root.join("src/c.rs"), c).unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    // No class may contain all three members — that is the whole point of the coherence split.
    assert!(
        res.classes.iter().all(|c| c.member_count < 3),
        "the over-merged chain must split — no class may keep all 3 members: {:?}",
        res.classes.iter().map(|c| c.member_count).collect::<Vec<_>>()
    );
    // Every returned class must be internally coherent (its aggregate min-pairwise ≥ θ).
    for class in &res.classes {
        assert!(
            class.cohesion_min_pairwise >= THETA - 1e-9,
            "a coherence-split class must be internally ≥ θ: got {}",
            class.cohesion_min_pairwise
        );
    }
    // After clique-cover split, both {A,B} and {B,C} are returned (B in both).
    assert_eq!(
        res.classes.len(),
        2,
        "the chain yields two coherent ≥2 classes: {{A,B}} and {{B,C}}"
    );
    for class in &res.classes {
        assert_eq!(class.member_count, 2, "each coherent class has 2 members");
    }

    // clones_for_symbol(A): A is in {A,B} only. The largest group containing A is {A,B} (refined).
    let by_a = db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::fa".into())).unwrap();
    let a_class = by_a.class.as_ref().expect("fa is in the coherent {A,B} sub-class");
    assert_eq!(a_class.member_count, 2, "clones_for_symbol(fa) returns A's coherent sub-class");
    // A's class must match one of the returned classes from find_clones.
    assert!(
        res.classes.iter().any(|c| c.class_key == a_class.class_key),
        "find_clones and clones_for_symbol must return the SAME coherent sub-class for A"
    );

    // clones_for_symbol(C): after the clique cover, C is in the coherent {B,C} sub-class (NOT a
    // singleton anymore), so the reverse lookup serves that refined 2-member class — no fallback to
    // the over-merged 3-member component.
    let by_c = db.clones_for_symbol(CloneSymbolSelector::Ref("src/c.rs::fc".into())).unwrap();
    let c_class = by_c.class.as_ref().expect("fc is in the coherent {B,C} sub-class");
    assert_eq!(
        c_class.member_count, 2,
        "clones_for_symbol(fc) returns C's coherent {{B,C}} sub-class"
    );
    assert!(c_class.refined, "the {{B,C}} sub-class is refined");

    let _ = fs::remove_dir_all(root);
}

/// #256 (R5c): the full clone path is DETERMINISTic on a synthetic transitive chain. The
/// edge-fed clique-cover split (over a bucketed edge subset) and the ROI sort must be stable so the
/// content-addressed refinement cache stays valid and the listing order does not flap between
/// identical rebuilds. We rebuild the SAME {A,B,C} chain fixture twice (independent indexes) and
/// assert `find_clones` returns byte-identical class order (class_key sequence) both times, and
/// `clones_for_symbol` agrees across runs.
#[test]
fn clone_split_full_path_is_deterministic() {
    // Reuse the empirically-tuned chain shape from coherence_split_applied_in_find_clones: A~B and
    // B~C clear θ, A/C is below θ, so the union-find component {A,B,C} is over-merged and must be
    // coherence-split into {A,B} and {B,C}.
    let core = "let c1 = ca(x); let c2 = cb(c1); let c3 = cc(c2);";
    let s1 = "if x > 0 { acc = p1(x); } else { acc = p2(x); } if acc > 1 { acc = p3(acc); } else \
              { acc = p4(acc); }";
    let s2 = "for it in xs { match it { 0 => acc += q1(it), 1 => acc += q2(it), _ => acc -= \
              q3(it) } } for jt in ys { match jt { 0 => acc += q4(jt), _ => acc -= q5(jt) } }";
    let sx = "while acc > 0 { acc = r1(acc); acc = r2(acc); acc = r3(acc); } while acc < 9 { acc \
              = r4(acc); acc = r5(acc); }";
    let sy = "loop { acc = s1f(acc); acc = s2f(acc); acc = s3f(acc); if acc == 0 { break; } } \
              loop { acc = s4f(acc); if acc < 0 { break; } }";
    let a = format!("pub fn fa(x: i32) -> i32 {{ {core} {s1} {sx} 0 }}\n");
    let b = format!("pub fn fb(x: i32) -> i32 {{ {core} {s1} {s2} 0 }}\n");
    let c = format!("pub fn fc(x: i32) -> i32 {{ {core} {s2} {sy} 0 }}\n");

    let build_and_list = || -> (Vec<String>, Option<String>) {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), a.as_str()).unwrap();
        fs::write(root.join("src/b.rs"), b.as_str()).unwrap();
        fs::write(root.join("src/c.rs"), c.as_str()).unwrap();
        let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
        let res = db
            .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
            .unwrap();
        let keys: Vec<String> = res.classes.iter().map(|cl| cl.class_key.clone()).collect();
        // clones_for_symbol on the chained subject B (which is in BOTH {A,B} and {B,C}).
        let by_b = db.clones_for_symbol(CloneSymbolSelector::Ref("src/b.rs::fb".into())).unwrap();
        let b_key = by_b.class.as_ref().map(|cl| cl.class_key.clone());
        let _ = fs::remove_dir_all(root);
        (keys, b_key)
    };

    let (keys1, b1) = build_and_list();
    let (keys2, b2) = build_and_list();
    assert_eq!(keys1, keys2, "find_clones class order must be identical across rebuilds");
    assert_eq!(b1, b2, "clones_for_symbol(fb) must resolve to the same class across rebuilds");
    // Sanity: the chain DID split (two coherent classes), so determinism is over a real split.
    assert_eq!(keys1.len(), 2, "the chain must split into two coherent classes: {keys1:?}");
}

/// #256 (R3): `clones-for` on a CHAINED symbol — the path #256 names broken — must serve the
/// subject's TIGHT coherent neighborhood, never the whole over-merged component. In the {A,B,C}
/// chain, a reverse lookup on the bridge symbol B returns a 2-member coherent class (one of {A,B} /
/// {B,C}), NOT a 3-member over-merged blob, and that class is internally coherent (≥ θ).
#[test]
fn clones_for_chained_symbol_serves_tight_neighborhood() {
    let core = "let c1 = ca(x); let c2 = cb(c1); let c3 = cc(c2);";
    let s1 = "if x > 0 { acc = p1(x); } else { acc = p2(x); } if acc > 1 { acc = p3(acc); } else \
              { acc = p4(acc); }";
    let s2 = "for it in xs { match it { 0 => acc += q1(it), 1 => acc += q2(it), _ => acc -= \
              q3(it) } } for jt in ys { match jt { 0 => acc += q4(jt), _ => acc -= q5(jt) } }";
    let sx = "while acc > 0 { acc = r1(acc); acc = r2(acc); acc = r3(acc); } while acc < 9 { acc \
              = r4(acc); acc = r5(acc); }";
    let sy = "loop { acc = s1f(acc); acc = s2f(acc); acc = s3f(acc); if acc == 0 { break; } } \
              loop { acc = s4f(acc); if acc < 0 { break; } }";
    let a = format!("pub fn fa(x: i32) -> i32 {{ {core} {s1} {sx} 0 }}\n");
    let b = format!("pub fn fb(x: i32) -> i32 {{ {core} {s1} {s2} 0 }}\n");
    let c = format!("pub fn fc(x: i32) -> i32 {{ {core} {s2} {sy} 0 }}\n");

    const THETA: f64 = 0.7;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), a.as_str()).unwrap();
    fs::write(root.join("src/b.rs"), b.as_str()).unwrap();
    fs::write(root.join("src/c.rs"), c.as_str()).unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let by_b = db.clones_for_symbol(CloneSymbolSelector::Ref("src/b.rs::fb".into())).unwrap();
    let b_class = by_b.class.as_ref().expect("fb is the bridge symbol — it has coherent peers");
    assert_eq!(
        b_class.member_count, 2,
        "the chained subject's class must be the TIGHT 2-member neighborhood, never the \
         over-merged 3-member component"
    );
    assert!(
        b_class.cohesion_min_pairwise >= THETA - 1e-9,
        "the served class must be internally coherent (≥ θ): got {}",
        b_class.cohesion_min_pairwise
    );
    let _ = fs::remove_dir_all(root);
}

/// #256 (R7) recall pin: a 7-copy structurally-identical clone family (the `collect_rows`-style
/// shape that motivated the issue) is STILL found after the over-merge fix, and the genuine clone
/// class is refined with full coverage + ranks well. The fix only ever PARTITIONS an over-merged
/// component into ≤-coherent classes; it must never drop a real multi-copy clone below the recall
/// floor. The seven bodies are byte-identical up to identifier names (alpha-renamed to ID<n>), so
/// they share one struct_hash → ONE coherent 7-member class via the struct-hash fast path.
#[test]
fn find_clones_recall_pin_seven_copy_clone_still_found() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Seven copies of the same DB-getter shape, differing only in fn + variable names. Same
    // structural token sequence ⇒ same struct_hash ⇒ one clone class (no member loss).
    for (i, (name, var)) in [
        ("collect_a", "a"),
        ("collect_b", "b"),
        ("collect_c", "c"),
        ("collect_d", "d"),
        ("collect_e", "e"),
        ("collect_f", "f"),
        ("collect_g", "g"),
    ]
    .iter()
    .enumerate()
    {
        fs::write(
            root.join(format!("src/f{i}.rs")),
            format!(
                "pub fn {name}(db: Db) -> i32 {{ let {var} = db.get(1); validate({var}); \
                 transform({var}); {var} + 1 }}\n"
            ),
        )
        .unwrap();
    }
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    // The 7-copy clone must be found as ONE class with all 7 members (recall preserved — no member
    // dropped by the split).
    let seven = res
        .classes
        .iter()
        .find(|c| c.member_count == 7)
        .expect("the 7-copy clone class must still be found at θ=0.7");
    // Genuine clone → refined with full coverage and a strong refactorability (the coverage gate
    // does NOT penalize it; it ranks well, not buried).
    assert!(seven.refined, "the 7-copy clone is refined");
    assert!(
        seven.anti_unify_coverage.unwrap_or(0.0) >= 0.5,
        "a byte-identical 7-copy clone must have high coverage, got {:?}",
        seven.anti_unify_coverage
    );
    assert!(seven.roi > 0.0, "a genuine clone keeps a positive ROI");

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: four renamed clones (same structure, different names) form ONE class that the refine
/// driver promotes to a refined class — `refined`, `class_kind == "refined_class"`, a near-perfect
/// `lcs_ratio`, `confidence == "high"`, `refactorability > 0.9`, `refine_mode == Some("baseline")`,
/// and a positive ROI (the refactorability multiplier replaces the cohesion one).
#[test]
fn find_clones_refines_a_clean_class() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(res.classes.len(), 1, "the four renamed clones are one class");
    let c = &res.classes[0];

    assert!(c.refined, "a clean class inside the refine budget must be refined");
    assert_eq!(c.class_kind, "refined_class");
    assert_eq!(c.refine_mode, Some("baseline"));
    let lcs = c.lcs_ratio.expect("a refined class carries an lcs_ratio");
    assert!(lcs > 0.95, "renamed clones are near-identical; lcs_ratio should be ~1.0, got {lcs}");
    assert_eq!(c.confidence.as_deref(), Some("high"), "near-perfect fidelity → high confidence");
    let refac = c.refactorability.expect("a refined class carries a refactorability");
    assert!(refac > 0.9, "refactorability should be high for a clean class, got {refac}");
    // ROI reflects refactorability: cross_module_spread × member_count × medoid × LBF × refac.
    let expected_roi = c.cross_module_spread as f64
        * c.member_count as f64
        * c.body_token_len_medoid as f64
        * c.roi_factors.load_bearing_factor
        * refac;
    assert!(
        (c.roi - expected_roi).abs() < 1e-6,
        "refined ROI must use refactorability: roi={} expected={expected_roi}",
        c.roi
    );

    let _ = fs::remove_dir_all(root);
}

/// Plan 4a: `clones_for_symbol` always refines the subject's class (when refine inputs are
/// available). A reverse lookup into the clean 4-member class returns a REFINED class with the
/// subject present.
#[test]
fn clones_for_symbol_returns_refined_class() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);

    let res =
        db.clones_for_symbol(CloneSymbolSelector::Ref("a/load_user.rs::load_user".into())).unwrap();
    let class = res.class.as_ref().expect("load_user is in the clone class");
    assert!(class.refined, "clones_for_symbol refines the subject's class");
    assert_eq!(class.class_kind, "refined_class");
    assert_eq!(class.refine_mode, Some("baseline"));
    assert!(class.lcs_ratio.is_some(), "a refined class carries an lcs_ratio");
    assert!(
        class.members.iter().any(|m| m.r#ref.ends_with("load_user.rs::load_user")),
        "the subject must appear in its own refined class: {:?}",
        class.members.iter().map(|m| &m.r#ref).collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(root);
}
