use super::*;

#[test]
fn indexing_writes_baseline_fingerprints_for_functions() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two near-identical functions (renamed) + one trivial one that must be skipped.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
         load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\npub fn \
         tiny() -> i32 { 0 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let conn = db.storage.connection();
    let fps: i64 = conn
        .query_row(
            "SELECT count(*) FROM symbol_fingerprints WHERE normalizer_kind='baseline'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fps, 2, "the two functions are fingerprinted; tiny() is below MIN_TOKENS");

    // The token bag rides each fingerprint row as a non-NULL `token_bag` BLOB (#231) — there is no
    // symbol_token_postings table any more. Both fingerprinted symbols carry a bag that decodes to
    // a non-empty `(token_hash, freq)` multiset matching their `token_len`.
    let bagged_symbols: i64 = conn
        .query_row(
            "SELECT count(*) FROM symbol_fingerprints
             WHERE normalizer_kind='baseline' AND token_bag IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bagged_symbols, 2, "both fingerprinted functions carry a non-NULL token_bag BLOB");

    // Decode each BLOB and confirm it is a real bag (lossless: token_len == sum of freqs, no
    // duplicate token_hash — the codec invariants exercised against indexed data).
    let mut stmt = conn
        .prepare(
            "SELECT token_len, token_bag FROM symbol_fingerprints
             WHERE normalizer_kind='baseline'",
        )
        .unwrap();
    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (token_len, blob) in &rows {
        let bag = rag_rat_clones::bag_blob::decode_token_bag(blob).expect("BLOB decodes");
        assert!(!bag.is_empty(), "a fingerprinted symbol has a non-empty bag");
        let total_freq: i64 = bag.iter().map(|&(_, f)| f).sum();
        assert_eq!(total_freq, *token_len, "token_len == sum of freqs (lossless bag)");
        let mut hashes: Vec<i64> = bag.iter().map(|&(h, _)| h).collect();
        let distinct = hashes.len();
        hashes.dedup();
        assert_eq!(hashes.len(), distinct, "no duplicate token_hash in the indexed bag");
    }

    // df is populated (recomputed from the BLOBs at finalize).
    let df_rows: i64 =
        conn.query_row("SELECT count(*) FROM clone_token_df", [], |r| r.get(0)).unwrap();
    assert!(df_rows > 0, "clone_token_df is populated during indexing");

    // The two functions are renamed clones, so they share tokens — at least one token's df >= 2.
    let max_df: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(df), 0) FROM clone_token_df WHERE normalizer_kind='baseline'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(max_df >= 2, "a token shared by both clones has df >= 2, got {max_df}");

    // Cascade: deleting a symbol drops its fingerprint row (the bag rides it as the BLOB column).
    conn.execute("DELETE FROM symbols", []).unwrap();
    let after_fps: i64 =
        conn.query_row("SELECT count(*) FROM symbol_fingerprints", [], |r| r.get(0)).unwrap();
    assert_eq!(after_fps, 0, "fingerprints (and their token_bag BLOBs) cascade on symbol delete");

    let _ = fs::remove_dir_all(&root);
}

/// T4 (#231): `refresh_clone_token_df` recomputed from the token-bag BLOBs equals the postings-era
/// `GROUP BY symbol_token_postings` semantics — df = the count of DISTINCT symbols whose decoded
/// bag contains each `(normalizer_kind, token_hash)`, with NO generated-file filter (R6). Build a
/// real index, then independently re-derive the expected df from the BLOBs and assert it equals the
/// persisted `clone_token_df` row-for-row.
#[test]
fn clone_token_df_recomputed_from_blobs_matches_postings_era() {
    // Asserts the whole-DB `clone_token_df` contents; opt out of the poison harness whose sibling
    // seeds a df row under its own repo_id.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two renamed clones (shared tokens → some df == 2) + one distinct function (its tokens → df
    // 1).
    fs::write(
        root.join("src/lib.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
         load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\npub fn \
         compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s += it * 2; } \
         s + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // Independently re-derive df from EVERY fingerprint BLOB (no generated filter — R6).
    let mut stmt =
        conn.prepare("SELECT normalizer_kind, token_bag FROM symbol_fingerprints").unwrap();
    let mut expected: std::collections::BTreeMap<(String, i64), i64> =
        std::collections::BTreeMap::new();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        let kind: String = row.get(0).unwrap();
        let Some(blob) = row.get::<_, Option<Vec<u8>>>(1).unwrap() else {
            continue;
        };
        let bag = rag_rat_clones::bag_blob::decode_token_bag(&blob).expect("decodes");
        for (token_hash, _freq) in bag {
            *expected.entry((kind.clone(), token_hash)).or_insert(0) += 1;
        }
    }
    assert!(!expected.is_empty(), "fixture produced fingerprints");

    // The persisted clone_token_df must match the independent recompute exactly.
    let mut df_stmt =
        conn.prepare("SELECT normalizer_kind, token_hash, df FROM clone_token_df").unwrap();
    let mut persisted: std::collections::BTreeMap<(String, i64), i64> =
        std::collections::BTreeMap::new();
    let mut df_rows = df_stmt.query([]).unwrap();
    while let Some(row) = df_rows.next().unwrap() {
        persisted.insert((row.get(0).unwrap(), row.get(1).unwrap()), row.get(2).unwrap());
    }
    assert_eq!(persisted, expected, "clone_token_df == distinct-symbol count per token from BLOBs");
    assert!(
        expected.values().any(|&d| d == 2),
        "the two renamed clones share at least one token (df == 2)"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn candidate_components_group_renamed_clones_and_exclude_unrelated() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/c.rs"),
        "pub fn parse_config(raw: String) -> Vec<u8> { let mut v = Vec::new(); for b in \
         raw.bytes() { v.push(b ^ 7); } v }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let components = db.candidate_clone_components().expect("components");
    // Exactly one component: the two renamed clones (a.rs + b.rs). parse_config in c.rs is
    // structurally unrelated and must not join the component.
    assert_eq!(components.len(), 1, "exactly one clone component: {components:?}");
    assert_eq!(components[0].len(), 2, "the component is the two renamed clones: {components:?}");

    let _ = fs::remove_dir_all(&root);
}

/// Adversarial containment (design rev-4 §8): a small function A whose entire token bag is
/// contained inside a much larger function B (A's body pasted into B amid other statements).
/// containment = overlap/min ≈ 1.0, but similarity = overlap/max ≈ 0.1 < THETA, so A and B are NOT
/// a whole-symbol clone — they must not land in a common component. (The size prune `min_len >=
/// ceil(THETA*max_len)` already excludes this pair before the exact verify.)
#[test]
fn candidate_components_reject_small_function_contained_in_large_one() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // A: a ~20-token real Rust function.
    let a_body = "let mut acc = 0; let p = compute(seed); acc += p; for q in items.iter() { acc \
                  += transform(q); } acc";
    std::fs::write(
        root.join("src/a.rs"),
        format!("pub fn small(seed: i32, items: Vec<i32>) -> i32 {{ {a_body} }}\n"),
    )
    .unwrap();
    // B: a ~200-token function that CONTAINS all of A's tokens (A's body pasted in) amid ~10x more
    // distinct statements, so B's token_len is roughly 10x A's. overlap/min(A) ≈ 1.0 but
    // overlap/max(B) ≈ 0.1.
    let mut filler = String::new();
    for i in 0..40 {
        filler.push_str(&format!(
            "let v{i} = step{i}(base{i}, factor{i}) + delta{i}; total += v{i} * weight{i} - \
             offset{i};\n"
        ));
    }
    std::fs::write(
        root.join("src/b.rs"),
        format!(
            "pub fn big(seed: i32, items: Vec<i32>, base: i32) -> i32 {{ let mut total = base; \
             {filler} {a_body}; total += acc; total }}\n"
        ),
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Sanity: both functions cleared MIN_TOKENS (so both are fingerprinted) and B is ~10x A.
    let conn = db.storage.connection();
    let lens: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT token_len FROM symbol_fingerprints WHERE normalizer_kind='baseline' ORDER \
                 BY token_len",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(lens.len(), 2, "both functions are fingerprinted: {lens:?}");
    assert!(
        lens[1] >= 5 * lens[0],
        "B is much larger than A so overlap/max stays below THETA: {lens:?}"
    );

    let components = db.candidate_clone_components().expect("components");
    assert!(
        components.is_empty(),
        "a small function contained in a large one is NOT a whole-symbol clone: {components:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// df is a selectivity hint only (design rev-4 §2, §8): emptying `clone_token_df` must NOT change
/// the components found. The deterministic token order falls back to `token_hash` via LEFT JOIN +
/// COALESCE, so no candidate is dropped — only the prefix prune loosens. Uses the
/// two-renamed-clones fixture.
#[test]
fn candidate_components_unchanged_when_clone_token_df_is_empty() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let before = db.candidate_clone_components().expect("components before df delete");
    assert_eq!(before.len(), 1, "baseline: one clone component: {before:?}");

    db.storage.connection().execute("DELETE FROM clone_token_df", []).unwrap();

    let after = db.candidate_clone_components().expect("components after df delete");
    assert_eq!(
        before, after,
        "df is selectivity-only: emptying clone_token_df must not change components"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The `files.generated = 0` predicate is a READ-SIDE filter: generated files are still
/// fingerprinted on write but their symbols must not appear in clone components. This test proves
/// the filter is doing the exclusion (not a missing fingerprint row).
#[test]
fn candidate_components_exclude_generated_files_via_read_filter() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // Two renamed clones — same fixture as
    // candidate_components_group_renamed_clones_and_exclude_unrelated.
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Baseline: both files non-generated — one clone component of 2.
    let before = db.candidate_clone_components().expect("components before marking generated");
    assert_eq!(before.len(), 1, "baseline: one clone component: {before:?}");
    assert_eq!(before[0].len(), 2, "baseline component has 2 members: {before:?}");

    // Mark b.rs as generated in the REAL base table (`temp.files` is a view; can't UPDATE it).
    // This tests that the `files.generated = 0` predicate in the read query does the exclusion;
    // the write rows (fingerprints/postings) are left intact to prove it's a read-side filter.
    let conn = db.storage.connection();
    let updated =
        conn.execute("UPDATE main.files SET generated = 1 WHERE path LIKE '%b.rs'", []).unwrap();
    assert_eq!(updated, 1, "exactly one file row marked generated");

    // b.rs's symbols MUST still have fingerprint rows (proves it's the read filter, not a missing
    // row).
    let fp_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM symbol_fingerprints sf
             JOIN symbols ON symbols.id = sf.symbol_id
             JOIN main.files ON main.files.id = symbols.file_id
             WHERE main.files.path LIKE '%b.rs' AND sf.normalizer_kind = 'baseline'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(fp_count > 0, "b.rs still has fingerprint rows after marking generated: {fp_count}");

    // After marking b.rs generated the read filter must drop it — no component pairs a with b.
    let after = db.candidate_clone_components().expect("components after marking generated");
    let has_b_pair = after.iter().any(|component| {
        // Any component that contained b.rs's symbol alongside a.rs's symbol would be size >= 2.
        // Since b.rs is the only partner for a.rs, if a.rs has no partner the component is gone.
        component.len() >= 2
    });
    assert!(
        !has_b_pair,
        "generated b.rs must be excluded from clone components by the read filter: {after:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #232 #6: a PATH-heuristic-generated file under a SOURCE target (`src/generated/*.rs`,
/// `is_generated_path` true, `kind = source`) gets full symbols but must NOT be fingerprinted at
/// index time — neither on a full rebuild NOR on a single-file heal. (`kind = Generated` files are
/// already symbol-empty, so the gate is needed only for the path-heuristic case.) This is pure
/// write-side storage hygiene — zero recall/precision effect (the read already filters
/// `generated = 0`); the assertion is on the absence of `symbol_fingerprints` ROWS.
#[test]
fn generated_files_are_not_fingerprinted_at_index_time() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/generated")).unwrap();
    // A normal source file (fingerprinted) and a path-heuristic-generated file under the SAME
    // source target. Both bodies clear MIN_TOKENS so the absence of a generated fp row is the gate,
    // not the size prune.
    std::fs::write(
        root.join("src/normal.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/generated/bindings.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let fp_rows_for = |db: &IndexDatabase, like: &str| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT count(*) FROM symbol_fingerprints sf
                 JOIN symbols ON symbols.id = sf.symbol_id
                 JOIN main.files ON main.files.id = symbols.file_id
                 WHERE main.files.path LIKE ?1 AND sf.normalizer_kind = 'baseline'",
                [like],
                |r| r.get(0),
            )
            .unwrap()
    };

    // The path-heuristic-generated file got symbols but NO fingerprint rows; the normal file did.
    assert!(fp_rows_for(&db, "%normal.rs") > 0, "normal source file must be fingerprinted");
    assert_eq!(
        fp_rows_for(&db, "%/generated/%"),
        0,
        "generated file must NOT be fingerprinted on a full rebuild"
    );
    // It DOES still get symbols (the gate is fingerprint-only, not symbol extraction).
    let gen_symbols: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT count(*) FROM symbols JOIN main.files ON main.files.id = symbols.file_id
             WHERE main.files.path LIKE '%/generated/%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        gen_symbols > 0,
        "generated file must still get symbols (only fingerprints are skipped)"
    );

    // Single-file heal path: re-index the generated file through heal_file → index_file →
    // store_symbol_fingerprints (gated). It must still write NO fingerprint rows.
    db.heal_file(std::path::Path::new("src/generated/bindings.rs")).unwrap();
    assert_eq!(
        fp_rows_for(&db, "%/generated/%"),
        0,
        "generated file must NOT be fingerprinted after a single-file heal"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #232 multi-language integration: a Rust + TS + Python repo with WITHIN-language planted clones
/// (Rust comment-only variant; TS function-valued declarators differing only in string contents;
/// Python comment-only variant) — exercises #1 (comments), #2a (TS strings) and #5 (TS
/// function-valued declarators) end-to-end through a real index. Asserts a within-language clone
/// component forms in EACH language and NO component mixes two languages (the #3 language
/// partition).
#[test]
fn multi_language_clone_integration_finds_within_language_no_cross() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("rs")).unwrap();
    fs::create_dir_all(root.join("ts")).unwrap();
    fs::create_dir_all(root.join("py")).unwrap();

    // Rust: two functions identical EXCEPT comments (comment-only clone → #1). >= MIN_TOKENS.
    fs::write(
        root.join("rs/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("rs/b.rs"),
        "pub fn load_order(s: Db) -> i32 {\n    // a different comment\n    let o = s.get(20); /* \
         x */ validate(o); o + 1 }\n",
    )
    .unwrap();

    // TS: two `const`-arrow function-valued declarators identical EXCEPT string contents (#5 +
    // #2a).
    fs::write(
        root.join("ts/a.ts"),
        "const load = (id) => { const row = get(id); const tag = label(\"alpha\"); send(row, \
         tag); return row; }\n",
    )
    .unwrap();
    fs::write(
        root.join("ts/b.ts"),
        "const fetch2 = (key) => { const item = get(key); const note = label(\"omega\"); \
         send(item, note); return item; }\n",
    )
    .unwrap();

    // Python: two functions identical EXCEPT comments (comment-only clone → #1). >= MIN_TOKENS.
    fs::write(
        root.join("py/a.py"),
        "def load_user(db):\n    u = db.get(10)\n    validate(u)\n    return u + 1\n",
    )
    .unwrap();
    fs::write(
        root.join("py/b.py"),
        "def load_order(s):\n    # a comment\n    o = s.get(20)  # trailing\n    validate(o)\n    \
         return o + 1\n",
    )
    .unwrap();

    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("rs")],
                include: vec!["rs/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
            ResolvedTarget {
                name: "typescript".to_string(),
                language: Language::TypeScript,
                directories: vec![PathBuf::from("ts")],
                include: vec!["ts/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
            ResolvedTarget {
                name: "python".to_string(),
                language: Language::Python,
                directories: vec![PathBuf::from("py")],
                include: vec!["py/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
        ],
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

    // Map each component's symbol ids → the set of languages it spans.
    let conn = db.storage.connection();
    let lang_of = |symbol_id: i64| -> String {
        conn.query_row(
            "SELECT files.language FROM symbols JOIN main.files ON main.files.id = symbols.file_id
             WHERE symbols.id = ?1",
            [symbol_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };

    let components = db.candidate_clone_components().unwrap();
    let mut langs_with_clone: std::collections::BTreeSet<String> = Default::default();
    for component in &components {
        let langs: std::collections::BTreeSet<String> =
            component.iter().map(|&id| lang_of(id)).collect();
        // No component may mix two languages (the #3 language partition).
        assert_eq!(
            langs.len(),
            1,
            "a clone component must be single-language (no cross-language pairs): {langs:?}"
        );
        langs_with_clone.insert(langs.into_iter().next().unwrap());
    }

    // A within-language clone was recalled in EACH of the three languages.
    for expected in ["rust", "typescript", "python"] {
        assert!(
            langs_with_clone.contains(expected),
            "expected a within-language clone in {expected}; got {langs_with_clone:?}"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

/// Max-denominator overlap gate regression: two structurally different functions whose
/// token_len ratio is ≥ θ (they SURVIVE the size prune) but whose token-overlap/max_len < θ
/// (the gate rejects them). This is distinct from the containment test
/// (`candidate_components_reject_small_function_contained_in_large_one`), which is eliminated
/// by the size prune alone — this fixture proves it is the overlap/max gate doing the work.
///
/// Fixture: `a` is a sequential let-chain; `b` is a loop+match accumulator. They are structurally
/// different enough that their shared tokens (keywords, operators, AST-node-kind tokens) fall well
/// below the overlap threshold, even though their token_lens are within the 1/θ ≈ 1.43x band.
#[test]
fn candidate_components_reject_partial_overlap_below_max_denominator_theta() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // a: sequential let-chain with five named sub-computations, returns a sum.
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn a(x: i32, y: i32) -> i32 { let p = alpha(x); let q = beta(y); let r = gamma(p); \
         let s = delta(q); let t = epsilon(r, s); p + q + r + s + t }\n",
    )
    .unwrap();
    // b: loop-based accumulator with a match arm — completely different control flow from a.
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn b(items: Vec<i32>, acc: i32) -> i32 { let mut total = acc; for item in \
         items.iter() { let v = process(item); match v { 0 => total += 1, _ => total += v } } if \
         total > 0 { total } else { -1 } }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Asserting the pair SURVIVES the size prune isolates the overlap/max gate as the reason for
    // exclusion (distinct from the 5× containment test, which the size prune kills).
    //
    // Measured token_lens: a=92, b=104.  ceil(0.7 * 104) = 73.  92 ≥ 73 → prune passes.
    // Overlap (Σ min(freq_a, freq_b)) = 51 < 73 → gate fails.  Values are asserted below so a
    // future fixture change that breaks the isolation is caught immediately.
    let conn = db.storage.connection();
    let lens: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT token_len FROM symbol_fingerprints WHERE normalizer_kind='baseline' ORDER \
                 BY token_len",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(lens.len(), 2, "both functions must be fingerprinted: {lens:?}");
    let min_len = lens[0];
    let max_len = lens[1];
    let threshold = (0.7_f64 * max_len as f64).ceil() as i64;
    assert!(
        min_len >= threshold,
        "pair must survive the size prune (min_len={min_len} >= ceil(0.7*max_len)={threshold}) so \
         the next assertion targets the overlap/max gate, not the prune"
    );

    let comps = db.candidate_clone_components().unwrap();
    assert!(
        comps.is_empty(),
        "a partial-overlap pair below overlap/max θ must NOT be a candidate (no regression to \
         containment): min_len={min_len} max_len={max_len} threshold={threshold} {comps:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// normalizer_version filter: after a NORM_VERSION bump the old rows are stale and the read
/// must ignore them. Simulate by writing rows at version N and then decrementing to N-1.
#[test]
fn candidate_read_ignores_stale_normalizer_version_rows() {
    let root = unique_temp_root();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    assert_eq!(
        db.candidate_clone_components().unwrap().len(),
        1,
        "renamed clones form one component at the current version"
    );
    // Simulate a NORM_VERSION bump that left old rows behind: rewrite both rows to an old version.
    db.storage
        .connection()
        .execute("UPDATE symbol_fingerprints SET normalizer_version = normalizer_version - 1", [])
        .unwrap();
    assert!(
        db.candidate_clone_components().unwrap().is_empty(),
        "stale-version fingerprints must be ignored by the read"
    );

    let _ = fs::remove_dir_all(&root);
}
