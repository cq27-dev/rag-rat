use super::*;

#[test]
fn worktree_overlay_resolution_does_not_corrupt_base_edges() {
    // #219 P1: an UNCHANGED committed caller's edge into a symbol the overlay renames must NOT be
    // rewritten by the overlay pass — the caller file's row is SHARED with the base scope, so an
    // overlay-scoped re-resolve against the (shadowed) overlay symbol set would corrupt the base
    // graph. The fix re-resolves ONLY the worktree's own overlay source rows.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // `caller.rs` calls `target_fn` (defined in `target.rs`). The caller is never touched on the
    // branch, so its committed row is shared between base and overlay scopes.
    fs::write(main.join("src/caller.rs"), "pub fn use_it() { target_fn(); }\n").unwrap();
    fs::write(main.join("src/target.rs"), "pub fn target_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The base edge resolves to a concrete target symbol; capture it.
    let base_target = calls_edge_target(&db, "src/caller.rs");
    assert!(base_target.is_some(), "base caller edge resolves to target_fn");

    // The branch RENAMES target_fn → renamed_fn (so target_fn no longer exists in the overlay's
    // symbol set), but leaves caller.rs untouched.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/target.rs"), "pub fn renamed_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "rename target"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "target.rs indexed as an overlay row");

    // Back in the base scope: the unchanged caller's edge must still resolve to the SAME base
    // target. Before the fix the overlay pass re-resolved the shared caller row against its own
    // symbol set (where target_fn is gone) and NULLed/retargeted it, corrupting the base graph.
    set_base_scope(&mut db, &main);
    assert_eq!(
        calls_edge_target(&db, "src/caller.rs"),
        base_target,
        "the overlay pass must not rewrite the shared base caller's resolved edge"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_does_not_pollute_or_clear_global_parser_failures() {
    // #219 review: `parser_failures` is keyed by `path` only and every reader counts it globally.
    // An overlay pass routes its files through the same write path, so (1) a BRANCH-ONLY syntax
    // error must not be recorded into the global table (it would show in base/sibling coverage),
    // and (2) an overlay pass over a path that is BROKEN in the base must not DELETE the base's
    // failure by bare path.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/clean.rs"), "pub fn clean_fn() {}\n").unwrap();
    fs::write(main.join("src/base_broken.rs"), "pub fn base_broken(").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    // The base has exactly one failure (base_broken.rs).
    assert_eq!(parser_failure_total(&db), 1, "base records its one parse failure");

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch BREAKS the previously-clean file AND fixes the base-broken one.
    fs::write(linked.join("src/clean.rs"), "pub fn clean_fn(").unwrap();
    fs::write(linked.join("src/base_broken.rs"), "pub fn now_ok() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch edits"]);

    let mut db = db;
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the branch's two changed files are indexed as overlay rows");

    // The global table is UNCHANGED by the overlay pass: the branch-only `clean.rs` failure was not
    // recorded, and the base `base_broken.rs` failure was not cleared by the overlay's same-path
    // re-index.
    assert_eq!(
        parser_failure_total(&db),
        1,
        "overlay neither pollutes nor clears the global parser_failures table"
    );
    set_base_scope(&mut db, &main);
    let base_failures = db.parser_failure_paths().unwrap();
    assert_eq!(base_failures.len(), 1);
    assert_eq!(
        base_failures[0].path, "src/base_broken.rs",
        "the base scope still reports its own parse failure, untouched by the overlay"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_read_chunk_returns_branch_text_not_main() {
    // #219 review: `read_chunk_current` revalidates a scoped chunk against `source_root` (the MAIN
    // checkout). When a branch differs from main but the chunk's anchor still validates against
    // main, the EXACT path re-sliced the chunk text out of MAIN's file — returning base text for a
    // branch chunk. The anchor hash is whitespace-NORMALIZED (lines trimmed, blanks dropped), so a
    // branch that differs from main ONLY in indentation anchors EXACT against main, then slices
    // main's de-indented bytes. The fix skips live revalidation under an overlay scope, returning
    // the stored branch-indexed text verbatim.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // Main: no indentation.
    let main_src = "pub fn marker() -> i32 {\nlet branch_witness = 1;\nbranch_witness\n}\n";
    fs::write(main.join("src/a.rs"), main_src).unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Branch: SAME normalized content (so the anchor matches EXACT against main), but distinctively
    // indented — the indentation is what proves whether the stored branch text or main's bytes win.
    let branch_src =
        "pub fn marker() -> i32 {\n        let branch_witness = 1;\n        branch_witness\n}\n";
    fs::write(linked.join("src/a.rs"), branch_src).unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    // index_worktree_overlay leaves the connection in the overlay scope.
    let overlay_chunk_id = scoped_chunk_id(&db, "src/a.rs");
    let chunk = db.read_chunk(overlay_chunk_id).unwrap().expect("overlay chunk readable");
    assert!(
        chunk.text.contains("        let branch_witness"),
        "read_chunk returns the BRANCH's indented text in the overlay scope (not main's \
         de-indented bytes via an EXACT anchor match), got: {:?}",
        chunk.text
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn refresh_worktree_overlays_reconciles_overlay_scope_embeddings() {
    // #219 review: `refresh_worktree_overlays` restored the base scope BEFORE the pass's reconcile,
    // so a NEW/CHANGED overlay chunk never got an embedding (worktree `semantic_search` stayed
    // BM25-only for branch content). The fix reconciles each CHANGED overlay inline, while scoped
    // to it. Uses the deterministic in-process HASH model (no download).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("src/a.rs"),
        "pub fn base_entry() {\n    // base content with enough detail to satisfy the embedding \
         policy minimum\n}\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    db.reconcile(None, Some(8)).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // A NEW branch-only file with enough text to be embedding-eligible.
    fs::write(
        linked.join("src/branch_new.rs"),
        "pub fn branch_entry() {\n    // branch-only content with enough detail to satisfy the \
         embedding policy minimum\n}\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "add branch file"]);

    let options = ai::ReconcileOptions { batch_size: Some(8), ..Default::default() };
    let budget = crate::watch::ReconcileBudget::new(options, std::time::Instant::now());
    // The pass refreshes the overlay AND reconciles its embeddings inline.
    let changed = crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        Some(&budget),
        &crate::watch::OverlayScope::All,
    );
    assert!(changed, "the overlay changed (a new branch file was indexed)");

    // In the overlay scope, the new branch file's chunk must carry a Current embedding — not be
    // left BM25-only. `refresh_worktree_overlays` restored the base scope, so re-enter the
    // overlay.
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    let embedded: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM chunk_embeddings ce
             JOIN chunks c ON c.id = ce.chunk_id
             JOIN files f ON f.id = c.file_id
             WHERE f.path = 'src/branch_new.rs' AND ce.status = 'Current'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(embedded >= 1, "the overlay's new chunk was reconciled into an embedding");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_branch_deleted_file_is_hidden_by_tombstone() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    fs::write(main.join("src/gone.rs"), "pub fn gone_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert!(path_in_scope(&db, "src/gone.rs"));

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::remove_file(linked.join("src/gone.rs")).unwrap();
    run_git(&linked, &["add", "-A"]);
    run_git(&linked, &["commit", "-q", "-m", "drop gone"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.tombstoned >= 1, "gone.rs written as a tombstone");

    // Tombstone hides the branch-deleted file; the untouched file falls through to the base.
    assert!(!path_in_scope(&db, "src/gone.rs"), "linked scope hides the branch-deleted file");
    assert!(path_in_scope(&db, "src/keep.rs"), "non-delta file falls through to the base");
    assert_eq!(names_in_scope(&db, "src/keep.rs"), vec!["keep_fn".to_string()]);

    set_base_scope(&mut db, &main);
    assert!(path_in_scope(&db, "src/gone.rs"), "the base scope still has the file");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_tombstones_an_ignored_replacement_of_a_base_file() {
    // #219 review: when a branch drops a base file from its tracked/indexable view but an IGNORED
    // file still sits at that path on disk, the candidate must be TOMBSTONED, not skipped. Before
    // the fix the on-disk-but-ignored path hit `continue`, so the overlay scope fell through to the
    // base row and queries returned a file the branch no longer presents.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/data.rs"), "pub fn base_data() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert!(path_in_scope(&db, "src/data.rs"));

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch git-rm's the tracked file (→ a deletion candidate in the tree-diff) AND ignores
    // the path, then drops a NEW untracked file at the same path: on disk it exists but is
    // gitignored, so it is NOT indexable — exactly the shadow-a-base-file-with-an-ignored-file
    // case.
    fs::remove_file(linked.join("src/data.rs")).unwrap();
    fs::write(linked.join(".gitignore"), "/src/data.rs\n").unwrap();
    run_git(&linked, &["add", "-A"]);
    run_git(&linked, &["commit", "-q", "-m", "drop + ignore data"]);
    fs::write(linked.join("src/data.rs"), "pub fn ignored_replacement() {}\n").unwrap();

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.tombstoned >= 1, "the ignored replacement of a base file is tombstoned");
    assert!(
        !path_in_scope(&db, "src/data.rs"),
        "the overlay hides the base file behind a tombstone (the branch's view dropped it)"
    );

    set_base_scope(&mut db, &main);
    assert!(path_in_scope(&db, "src/data.rs"), "the base scope still has the file");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_reads_do_not_heal_against_main() {
    // #219 review: read tools (symbol_lookup / impact / search) revalidated overlay rows against
    // `source_root` (the MAIN checkout). A branch that changes more than
    // MAX_AUTO_HEAL_FILES_PER_CALL files looks entirely stale vs main, so `symbol_candidates`'
    // matched-file heal tripped `NeedsReindex` (and `heal_file` no-ops under an overlay
    // anyway). The overlay is authoritative, so the staleness check must be skipped under a
    // linked-overlay scope.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // More than the heal cap (4) so an unguarded check would treat every branch file as stale.
    for i in 0..6 {
        fs::write(main.join(format!("src/f{i}.rs")), format!("pub fn shared_{i}() {{}}\n"))
            .unwrap();
    }
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Every file differs from main on the branch, and one carries a NEW symbol to look up.
    for i in 0..6 {
        fs::write(
            linked.join(format!("src/f{i}.rs")),
            format!("pub fn shared_{i}() {{}}\npub fn branch_only_{i}() {{}}\n"),
        )
        .unwrap();
    }
    run_git(&linked, &["add", "-A"]);
    run_git(&linked, &["commit", "-q", "-m", "branch changes every file"]);

    // Leaves the connection in the overlay scope.
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    let selector = rag_rat_query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("branch_only_0".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    };
    // Unguarded, this raised NeedsReindex (6 > cap 4 stale-vs-main files); guarded, it resolves the
    // branch symbol cleanly and flags nothing stale.
    let lookup = db.symbol_candidates(&selector, false).unwrap();
    assert!(
        lookup.candidates.iter().any(|c| c.name == "branch_only_0"),
        "the overlay symbol resolves without a main-root stale heal: {:?}",
        lookup.candidates.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert!(
        lookup.stale_files.is_empty(),
        "no overlay file is flagged stale against main: {:?}",
        lookup.stale_files
    );
    // Search must also not raise NeedsReindex under the overlay scope.
    db.search("shared_0", 10, false).expect("search succeeds under the overlay scope");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_untracked_linked_file_appears() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    // Untracked new file in the linked checkout (no branch commit).
    fs::write(linked.join("src/new.rs"), "pub fn new_fn() {}\n").unwrap();

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(names_in_scope(&db, "src/new.rs"), vec!["new_fn".to_string()]);

    set_base_scope(&mut db, &main);
    assert!(!path_in_scope(&db, "src/new.rs"), "the untracked file is not in the base scope");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_reads_uncommitted_linked_edit_not_head() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    // Linked HEAD == base; only a DIRTY (uncommitted) edit in the linked working tree.
    fs::write(linked.join("src/a.rs"), "pub fn dirty_fn() {}\n").unwrap();

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["dirty_fn".to_string()],
        "overlay reads the linked WORKING tree, not the linked HEAD tree"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_query_routing_selects_scope() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // The routing entry the MCP/query path uses: None -> base, a valid linked sibling -> its
    // overlay, an unreadable/foreign path -> base (never the wrong repo).
    db.use_worktree_scope(&main, None).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["base_fn".to_string()]);

    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["linked_fn".to_string()]);

    db.use_worktree_scope(&main, Some(Path::new("/"))).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["base_fn".to_string()],
        "an unreadable worktree path falls back to the base scope"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
