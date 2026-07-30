use super::*;

/// Partner hydration is memoized per coherent subclass, so a file contributing SEVERAL symbols to
/// one clone class fetches its members once instead of once per symbol. A cache keyed wrongly
/// would be silent — each region would render some other subclass's members — so assert every
/// region's partners are exactly its own class minus itself.
#[test]
fn several_symbols_from_one_file_in_one_class_each_get_their_own_partners() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two members in the requested file, one in a sibling: the requested file's two regions must
    // each see the other two members, never themselves.
    fs::write(
        root.join("src/pair.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
         load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/other.rs"),
        "pub fn load_item(cache: Db) -> i32 { let i = cache.get(30); validate(i); i + 1 }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");

    let res = db.lens_file_clones("src/pair.rs", 0.7, 0).unwrap();
    assert_eq!(res.clone_regions.len(), 2, "both in-file members anchor a region: {res:?}");
    for region in &res.clone_regions {
        let partners: Vec<&str> =
            region.partners.iter().filter_map(|p| p.symbol.as_deref()).collect();
        assert!(
            !partners.contains(&region.symbol.as_str()),
            "a region must never partner with itself: {region:?}"
        );
        assert_eq!(partners.len(), 2, "each region sees the other two class members: {region:?}");
    }
    let symbols: Vec<&str> = res.clone_regions.iter().map(|r| r.symbol.as_str()).collect();
    assert!(symbols.contains(&"load_user") && symbols.contains(&"load_order"), "{symbols:?}");
}

/// The refinement payload the editor renders — template, variation points, proposed signature,
/// and the refactorability scores — is a READ-THROUGH of the `clone_refinements` cache that a
/// `find_clones` pass populates. The lens never cold-computes it (that would turn an editor read
/// into a write), so an unrefined index must yield a region with no payload, and a refined one
/// must carry every field through to the region intact.
#[test]
fn lens_file_clones_carries_the_cached_refinement_payload() {
    let root = unique_temp_root();
    let db = write_four_renamed_clones(&root);
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        "Complete",
        "the lens reads the persisted graph — it must build Complete"
    );

    let cold = db.lens_file_clones("a/load_user.rs", 0.7, 0).unwrap();
    assert_eq!(cold.clone_regions.len(), 1, "the renamed clones form one class: {cold:?}");
    assert!(
        cold.clone_regions[0].refine.is_none(),
        "an editor read must not cold-compute a refinement: {:?}",
        cold.clone_regions[0]
    );

    // A `find_clones` pass is what populates the cache the lens then serves.
    let found = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(found.classes[0].refined, "the fixture class must refine");

    let warm = db.lens_file_clones("a/load_user.rs", 0.7, 0).unwrap();
    let region = &warm.clone_regions[0];
    let refine = region.refine.as_ref().expect("a cached refinement must reach the lens");
    assert!(!refine.template.is_empty(), "the editor renders the template: {refine:?}");
    assert!(!refine.confidence.is_empty(), "the editor labels confidence: {refine:?}");
    assert!(
        refine.anti_unify_coverage > 0.0 && refine.anti_unify_coverage <= 1.0,
        "coverage is a ratio: {refine:?}"
    );
    assert!(refine.lcs_ratio > 0.0 && refine.lcs_ratio <= 1.0, "lcs is a ratio: {refine:?}");
    assert!(refine.refactorability > 0.0, "a refined class is actionable: {refine:?}");
    assert!(
        !refine.variation_points.is_null(),
        "renamed clones vary in their identifiers: {refine:?}"
    );
}

/// The lens clones composition mirrors `clones_for_symbol`'s class rules on
/// the persisted graph: coherent class, partners with similarities, dual
/// anchoring for same-file pairs, and the min_tokens actionability filter.
#[test]
fn lens_file_clones_serves_coherent_class_with_partners() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/c.rs"),
        "pub fn parse_config(raw: String) -> Vec<u8> { let mut v = Vec::new(); for b in \
         raw.bytes() { v.push(b ^ 7); } v }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let no_graph = db.lens_file_clones("src/a.rs", 0.7, 0).unwrap();
    assert!(
        serde_json::to_value(no_graph).unwrap()["clone_graph"].is_null(),
        "the shim contract represents an absent generation as clone_graph: null"
    );
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        "Complete",
        "the lens reads the persisted graph — it must build Complete"
    );

    let ro = IndexDatabase::try_open_config_read_only(&config)
        .unwrap()
        .expect("a current index must open read-only");
    let cold_read = ro.lens_file_clones("src/a.rs", 0.7, 0).unwrap();
    assert_eq!(
        cold_read.clone_regions.len(),
        1,
        "a missing refinement cache row must not turn the lens read into a write"
    );

    let res = db.lens_file_clones("src/a.rs", 0.7, 0).unwrap();
    assert!(res.clone_graph.eligible, "Complete generation is eligible");
    assert_eq!(res.clone_regions.len(), 1, "one cloned symbol in a.rs: {res:?}");
    let region = &res.clone_regions[0];
    assert_eq!(region.symbol, "load_user");
    assert!(!region.class_key.is_empty());
    assert_eq!(region.partners.len(), 1, "only the renamed clone partners: {region:?}");
    let partner = &region.partners[0];
    assert_eq!(partner.symbol.as_deref(), Some("load_order"));
    assert_eq!(partner.path, "src/b.rs");
    // EXACTLY 1.0, not merely ≥ θ. `struct_hash` and `token_bag` are both derived from the one
    // normalized token sequence in `fingerprint_symbol`, so two symbols that are identical after
    // normalization — these differ only in identifier names — have equal bags and equal
    // `token_len`, and `overlap / max_len` is already 1. That is why the similarity closure needs
    // no structural-hash special case to score an exact clone as exact. A change that decoupled
    // the bag from the hash (raw tokens, a capped bag) would break it here first.
    assert_eq!(
        partner.similarity, 1.0,
        "identical-after-normalization clones score exact: {partner:?}"
    );

    // Dual anchoring: the same-file sibling is itself a region with the mirror partner.
    let res_b = db.lens_file_clones("src/b.rs", 0.7, 0).unwrap();
    assert_eq!(res_b.clone_regions.len(), 1);
    assert_eq!(res_b.clone_regions[0].partners[0].symbol.as_deref(), Some("load_user"));
    assert_eq!(
        res_b.clone_regions[0].class_key, region.class_key,
        "both anchors of the pair share one class identity"
    );
    assert_eq!(
        res_b.clone_regions[0].class_id, region.class_id,
        "class ids are stable across per-file requests"
    );
    let cached_graph = db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();

    // The unrelated function has no coherent class.
    let res_c = db.lens_file_clones("src/c.rs", 0.7, 0).unwrap();
    assert!(res_c.clone_regions.is_empty(), "no clones for c.rs: {res_c:?}");

    // The actionability filter hides small classes honestly.
    let res_h = db.lens_file_clones("src/a.rs", 0.7, 10_000).unwrap();
    assert!(res_h.clone_regions.is_empty());
    assert_eq!(res_h.clone_graph.hidden_low_value_classes, 1);
    let filtered_graph = db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();
    assert!(
        std::sync::Arc::ptr_eq(&cached_graph, &filtered_graph),
        "min_tokens filters cached classes without rebuilding the repository graph"
    );

    db.lens_file_clones("src/a.rs", 0.71, 0).unwrap();
    let changed_theta_graph =
        db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();
    assert!(
        !std::sync::Arc::ptr_eq(&cached_graph, &changed_theta_graph),
        "theta changes invalidate the cached edge set"
    );

    let normalized = db.lens_file_clones("src/a.rs", 0.7, -10).unwrap();
    assert_eq!(normalized.clone_graph.min_tokens, 0);
    assert!(db.lens_file_clones("src/a.rs", f64::NAN, 0).is_err());

    db.lens_file_clones("src/a.rs", 0.71, 0).unwrap();
    let graph_before_mutation =
        db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();
    db.connection()
        .execute(
            "UPDATE clone_graph_generations SET delta_files_applied = delta_files_applied + 1 \
             WHERE generation = ?1",
            [normalized.clone_graph.generation.unwrap()],
        )
        .unwrap();
    db.lens_file_clones("src/a.rs", 0.71, 0).unwrap();
    let graph_after_mutation =
        db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();
    assert!(
        !std::sync::Arc::ptr_eq(&graph_before_mutation, &graph_after_mutation),
        "in-place graph mutation epochs invalidate cached components"
    );
}

#[test]
fn lens_clone_graph_uses_scope_correct_live_fallback_for_a_linked_overlay() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("crate/src")).unwrap();
    // `Config::load` canonicalizes the root and the overlay depends on it: the config subdir is
    // found by stripping `config.root` against a canonicalized workdir, and a non-canonical root
    // makes that strip fail, scoping the refresh to the wrong directory so it sees no change at
    // all. The shared fixture builds a `Config` directly and skips that step, which is invisible
    // wherever the temp directory is already canonical and breaks where it is not — macOS
    // resolving `/var` to `/private/var`, Windows expanding 8.3 names. Doing it fixture-wide is
    // the real fix and is #1027; here it is done locally so this test states what it means to.
    let main = rag_rat_base::paths::canonicalize_or_simplified(&main);
    fs::write(main.join("crate/src/base.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.join("crate"), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    db.precompute_clone_graph(None).unwrap();
    let before_overlay = db.lens_version().unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "lens-overlay", linked.to_str().unwrap()]);
    fs::write(
        linked.join("crate/src/overlay_a.rs"),
        "pub fn overlay_user(db: Db) -> i32 { let value = db.get(1); validate(value); value + 1 \
         }\n",
    )
    .unwrap();
    fs::write(
        linked.join("crate/src/overlay_b.rs"),
        "pub fn overlay_order(store: Db) -> i32 { let item = store.get(2); validate(item); item + \
         1 }\n",
    )
    .unwrap();
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    // The overlay must have TARGETED the linked checkout. `index_worktree_overlay` returns `Ok(())`
    // without doing anything when the path does not resolve to a sibling worktree, so asserting
    // only on the revision below cannot tell "resolved and found no change" from "never resolved".
    assert_eq!(
        db.active_worktree_id,
        rag_rat_base::paths::canonicalize_or_simplified(&linked).display().to_string(),
        "the overlay refresh must scope itself to the linked checkout"
    );
    let after_overlay = db.lens_version().unwrap();
    assert_ne!(
        before_overlay.revision, after_overlay.revision,
        "materializing a linked-worktree edit must invalidate connected Lens clients"
    );
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        after_overlay.revision,
        db.lens_version().unwrap().revision,
        "an unchanged overlay refresh must remain write-idle for Lens"
    );

    let overlay_chunk_id: i64 = db
        .connection()
        .query_row(
            "SELECT chunk.id FROM chunks chunk
             JOIN files file ON file.id = chunk.file_id
             WHERE file.path = 'src/overlay_a.rs' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let overlay_chunk = db.lens_chunk_text(overlay_chunk_id).unwrap().unwrap();
    assert!(overlay_chunk.text.contains("pub fn overlay_user"));

    let treemap = db.lens_treemap().unwrap().files;
    let overlay_a = treemap.iter().find(|file| file.path == "src/overlay_a.rs").unwrap();
    assert_eq!(overlay_a.dup_partners, 1, "branch-only clone must count as a hotspot");
    assert_eq!(overlay_a.dup_max_similarity, 1.0, "structural-hash edges are exact matches");
    let treemap_graph = db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();

    let result = db.lens_file_clones("src/overlay_a.rs", 0.9, 0).unwrap();
    assert!(result.clone_graph.eligible);
    assert_eq!(result.clone_regions.len(), 1, "branch-only clone must be served: {result:?}");
    assert_eq!(result.clone_regions[0].partners.len(), 1);
    assert_eq!(result.clone_regions[0].partners[0].path, "src/overlay_b.rs");
    let wire = serde_json::to_value(result).unwrap();
    assert_eq!(wire["clone_graph"]["eligible"], true);
    assert_eq!(
        db.lens_clone_graph_cache.0.lock().unwrap().len(),
        2,
        "treemap and configured file-clone theta variants coexist"
    );
    db.lens_treemap().unwrap();
    let cache = db.lens_clone_graph_cache.0.lock().unwrap();
    assert_eq!(cache.len(), 2, "re-reading the treemap must hit its theta variant");
    assert!(std::sync::Arc::ptr_eq(&cache[0].data, &treemap_graph));
    drop(cache);

    db.use_worktree_scope(&config.root, None).unwrap();
    assert!(
        db.lens_treemap().unwrap().files.iter().all(|file| !file.path.starts_with("src/overlay_")),
        "linked-worktree treemap rows must not leak into main"
    );
    assert!(
        db.lens_chunk_text(overlay_chunk_id).unwrap().is_none(),
        "materializing and versioning a sibling overlay must not leak its rows into main"
    );
    assert!(
        db.lens_file_clones("src/overlay_a.rs", 0.7, 0).unwrap().clone_regions.is_empty(),
        "the main checkout must not reuse the linked-worktree clone graph cache"
    );
}

#[test]
fn lens_overlay_clone_cache_invalidates_on_byte_identical_target_drift() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("src/a.h"),
        "int load_user(Db db) { int value = db_get(db, 1); validate(value); return value + 1; }\n",
    )
    .unwrap();
    fs::write(
        main.join("src/b.h"),
        "int load_order(Db store) { int item = db_get(store, 2); validate(item); return item + 1; \
         }\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let base_config = source_config_dirs(main.clone(), Language::C, &["src"]);
    let mut db = IndexDatabase::rebuild(&base_config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "lens-relanguage", linked.to_str().unwrap()]);
    let branch_config = source_config_dirs(main.clone(), Language::Cpp, &["src"]);
    db.index_worktree_overlay(&branch_config, &linked, &mut |_| {}).unwrap();
    let before_revision = db.content_revision().unwrap();
    assert_eq!(db.lens_file_clones("src/a.h", 0.7, 0).unwrap().clone_regions.len(), 1);
    let source_graph = db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();

    let mut tests_config = source_config_dirs(main.clone(), Language::Cpp, &["src"]);
    tests_config.targets[0].kind = TargetKind::Tests;
    db.index_worktree_overlay(&tests_config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.content_revision().unwrap(),
        before_revision,
        "target-identity replacement is intentionally byte-identical"
    );
    let language: String = db
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/a.h'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(language, "cpp");

    let result = db.lens_file_clones("src/a.h", 0.7, 0).unwrap();
    assert_eq!(result.clone_regions.len(), 1, "the test-target graph must replace cached ids");
    let tests_graph = db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();
    assert!(
        !std::sync::Arc::ptr_eq(&source_graph, &tests_graph),
        "the files sequence must invalidate byte-identical target drift"
    );

    db.connection()
        .execute(
            "UPDATE main.files SET generated = 1 WHERE path = 'src/b.h' AND worktree_id = ?1",
            [&db.active_worktree_id],
        )
        .unwrap();
    db.connection()
        .execute("UPDATE index_meta SET value = 'next' WHERE key = ?1", [
            crate::index::GENERATED_FLAGS_VERSION_KEY,
        ])
        .unwrap();
    assert_eq!(db.content_revision().unwrap(), before_revision);
    let generated_result = db.lens_file_clones("src/a.h", 0.7, 0).unwrap();
    assert!(generated_result.clone_regions.is_empty(), "generated partners leave the live corpus");
    let generated_graph = db.lens_clone_graph_cache.0.lock().unwrap().last().unwrap().data.clone();
    assert!(
        !std::sync::Arc::ptr_eq(&tests_graph, &generated_graph),
        "the generated-flags version must invalidate in-place membership changes"
    );

    db.use_worktree_scope(&main, None).unwrap();
    let base_language: String = db
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/a.h'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(base_language, "c", "the linked C++ target must not alter main scope");
}
