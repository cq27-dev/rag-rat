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

#[test]
fn worktree_overlay_gc_keeps_a_live_worktrees_overlay() {
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

    // Reset to the BASE scope so the overlay is kept only via `live_worktree_contexts` (the active-
    // context fallback in garbage_collect would otherwise mask a worktree_id mismatch).
    set_base_scope(&mut db, &main);
    let before = overlay_row_count(&db);
    assert!(before > 0, "overlay rows exist before GC");

    db.garbage_collect().unwrap();
    assert_eq!(
        overlay_row_count(&db),
        before,
        "GC keeps a live worktree's overlay (the overlay worktree_id matches the GC live set)"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_gc_prunes_a_removed_worktrees_overlay() {
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

    set_base_scope(&mut db, &main);
    assert!(overlay_row_count(&db) > 0);

    // Remove the worktree → it leaves the `live_worktree_contexts` set → GC prunes its overlay.
    run_git(&main, &["worktree", "remove", "--force", linked.to_str().unwrap()]);
    db.garbage_collect().unwrap();
    assert_eq!(overlay_row_count(&db), 0, "GC prunes a removed worktree's overlay");

    // Post-condition: a repo-scoped GC must not touch a SIBLING repo's rows (round-6 harness).
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());
    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_rename_is_delete_old_plus_add_new() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/old.rs"), "pub fn moved_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    run_git(&linked, &["mv", "src/old.rs", "src/new.rs"]);
    run_git(&linked, &["commit", "-q", "-m", "rename"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // Linked scope: the old path is tombstoned (hidden), the new path carries the moved symbol.
    assert!(!path_in_scope(&db, "src/old.rs"), "renamed-from path is hidden in the worktree scope");
    assert!(path_in_scope(&db, "src/new.rs"));
    assert_eq!(names_in_scope(&db, "src/new.rs"), vec!["moved_fn".to_string()]);

    // Base scope is unchanged: old exists, new does not.
    set_base_scope(&mut db, &main);
    assert!(path_in_scope(&db, "src/old.rs"));
    assert!(!path_in_scope(&db, "src/new.rs"));

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn maintenance_pass_refreshes_a_linked_worktree_overlay() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    // A watcher maintenance pass auto-refreshes the overlay — no manual `index --worktree`.
    crate::watch::maintenance_pass(&config, false).unwrap();

    // A fresh reader on the shared DB: config-bearing (post-A7 a bare open refuses the
    // multi-repo shape the poison sibling makes real on this git fixture).
    let mut db = IndexDatabase::open_config(&config).unwrap();
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["linked_fn".to_string()],
        "the maintenance pass populated the worktree overlay"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_reindex_is_idle_safe() {
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

    let first = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(first.indexed >= 1, "the first overlay pass indexes the delta");
    // A re-run on an UNCHANGED worktree must be a no-op (sha-skip + tombstone-exists + gated
    // edge-resolve), so the watcher can refresh every pass without churn (#63 idle backstop).
    let second = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        (second.indexed, second.tombstoned, second.pruned),
        (0, 0, 0),
        "an unchanged worktree re-index writes nothing"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_honors_gitignore_and_refreshes_on_change() {
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
    // A tracked-but-gitignored file on the branch (force-added past the ignore rule), so it appears
    // in the committed tree-diff yet must be excluded by the worktree's `.gitignore` — parity with
    // the base walker, which the gix status path alone wouldn't enforce for a tracked file.
    fs::write(linked.join(".gitignore"), "/src/ignored.rs\n").unwrap();
    fs::write(linked.join("src/ignored.rs"), "pub fn ignored_fn() {}\n").unwrap();
    run_git(&linked, &["add", ".gitignore"]);
    run_git(&linked, &["add", "-f", "src/ignored.rs"]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(
        !path_in_scope(&db, "src/ignored.rs"),
        "a gitignored worktree file is not overlaid (parity with the base walker)"
    );

    // Remove the ignore rule on disk and re-index → the overlay now picks it up (a `.gitignore`
    // change is honored because the matcher is recompiled each pass).
    fs::write(linked.join(".gitignore"), "").unwrap();
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/ignored.rs"),
        vec!["ignored_fn".to_string()],
        "removing the ignore rule refreshes the overlay to include the file"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_pending_embeddings_are_detectable_for_retry() {
    // #219 review (3440746687): a per-overlay embedding reconcile that returns `Partial` (the
    // shared time budget ran out mid-pass) leaves the overlay's remaining chunks un-embedded.
    // The next pass sees the overlay rows as unchanged and would skip the embed forever. The
    // watcher retries on a positive `pending_embedding_jobs` count IN THE OVERLAY SCOPE — this
    // asserts that count is non-zero for an overlay whose chunks haven't been embedded, and
    // zero once they have.
    // Function bodies long enough to clear the embedding eligibility floor (MIN_EMBEDDING_CHARS).
    let base_src = r#"pub fn base_fn(input: u32) -> u32 {
    let doubled = input.wrapping_mul(2);
    let offset = doubled.wrapping_add(7);
    offset.wrapping_sub(input)
}
"#;
    let branch_src = r#"pub fn linked_fn(input: u32) -> u32 {
    let tripled = input.wrapping_mul(3);
    let offset = tripled.wrapping_add(11);
    offset.wrapping_sub(input)
}
"#;
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), base_src).unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    // The base has at least one embeddable chunk before reconcile (so the test isn't vacuous)...
    set_base_scope(&mut db, &main);
    assert!(db.pending_embedding_jobs().unwrap() > 0, "base has an embeddable chunk to begin with");
    // ...and embedding the base clears its backlog.
    db.reconcile_with_options_progress(ai::ReconcileOptions::default(), |_| {}).unwrap();
    set_base_scope(&mut db, &main);
    assert_eq!(db.pending_embedding_jobs().unwrap(), 0, "base scope is fully embedded");

    // A linked worktree modifies the file → the overlay carries a NEW, un-embedded chunk.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), branch_src).unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    // index_worktree_overlay leaves the connection scoped to the overlay (and does NOT embed).
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(
        db.pending_embedding_jobs().unwrap() > 0,
        "the overlay's un-embedded chunk is detectable as pending in the overlay scope (retry \
         gate)",
    );

    // After reconciling in the overlay scope, the backlog is cleared — a later pass won't re-run.
    db.reconcile_with_options_progress(ai::ReconcileOptions::default(), |_| {}).unwrap();
    assert_eq!(
        db.pending_embedding_jobs().unwrap(),
        0,
        "once embedded, the overlay reports no pending jobs (idle-safe, no perpetual retry)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_fts_freshness_revision_is_scope_invariant() {
    // #219 review (3440746692): `content_revision` (the FTS freshness digest) read the SCOPED
    // `files` view, so the global `fts_source_revision` `sync_fts` recorded under a linked-overlay
    // scope differed from the base-scope digest. Interleaved base/overlay reads then each saw the
    // global revision as stale and rebuilt the global FTS, alternating forever. The digest must be
    // GLOBAL (over `main.files`) so it is identical across scopes.
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

    // The digest computed under the OVERLAY scope (left active by index_worktree_overlay)...
    let overlay_revision = db.content_revision().unwrap();
    // ...must equal the digest under the BASE scope: it's a GLOBAL digest, not a per-scope one.
    set_base_scope(&mut db, &main);
    let base_revision = db.content_revision().unwrap();
    assert_eq!(
        overlay_revision, base_revision,
        "the FTS freshness digest is global, so it can't alternate as scopes interleave",
    );

    // And it matches the stored `fts_source_revision` `sync_fts` wrote during the overlay refresh,
    // so a base read sees FTS as fresh (no rebuild) rather than perpetually stale.
    assert_eq!(
        db.meta("fts_source_revision").unwrap().as_deref(),
        Some(base_revision.as_str()),
        "fts_source_revision (GLOBAL, in index_meta) recorded during the overlay pass matches the \
         global digest",
    );
    assert!(!db.fts_dirty().unwrap(), "the overlay refresh left FTS clean, not dirty");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_tombstones_a_base_file_a_gitignore_only_change_now_ignores() {
    // #219 review (3440746674): when the branch's ONLY change is a `.gitignore` rule, the
    // tree-diff/status candidates contain just `.gitignore` — an UNCHANGED base file the rule now
    // ignores is never visited, so its (now stale) base row keeps showing in the worktree scope.
    // The ignore-flip expansion must add that base file as a candidate so it is tombstoned.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The branch's ONLY change is a `.gitignore` rule that ignores the (unchanged) `src/a.rs`.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join(".gitignore"), "/src/a.rs\n").unwrap();
    run_git(&linked, &["add", ".gitignore"]);
    run_git(&linked, &["commit", "-q", "-m", "ignore a.rs"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.tombstoned >= 1, "the ignore-only change tombstones the now-ignored base file");
    assert!(
        !path_in_scope(&db, "src/a.rs"),
        "a base file the branch's `.gitignore` now ignores is hidden in the worktree scope, not \
         served from its stale base row",
    );
    // The sibling the rule does NOT touch is untouched (still served from its shared base row).
    assert_eq!(
        names_in_scope(&db, "src/keep.rs"),
        vec!["keep_fn".to_string()],
        "an unaffected base file is still served (no over-tombstoning)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_refreshed_with_the_branch_config_keeps_a_branch_only_target() {
    // #219 review (3440746699): the main watcher/maintenance process sweeps every linked worktree
    // with ITS OWN config. A branch whose `rag-rat.toml` ADDS a target (`extra/`) must be refreshed
    // with the branch's targets (`Config::for_linked_worktree_overlay`), or the overlay rows a
    // branch-launched hook indexed for `extra/` are filtered out of the delta and PRUNED by the
    // sweep. This asserts the overlay row survives a sweep that uses the branch config.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    // Main indexes only `src`.
    fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    // The sweeping process's config is main's: `src` only.
    let sweep_config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&sweep_config).unwrap();

    // A branch adds an `extra/` target and a file in it, with its own `rag-rat.toml`.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::create_dir_all(linked.join("extra")).unwrap();
    fs::write(linked.join("extra/more.rs"), "pub fn extra_fn() {}\n").unwrap();
    fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\", \"extra\"]\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds extra"]);

    // A branch-launched hook indexes the overlay WITH the branch config — `extra/more.rs` is
    // overlaid.
    let branch_config = sweep_config.for_linked_worktree_overlay(&linked);
    db.index_worktree_overlay(&branch_config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        names_in_scope(&db, "extra/more.rs"),
        vec!["extra_fn".to_string()],
        "the branch-only target file is overlaid by the branch-config pass",
    );

    // The MAIN sweep refreshes the same worktree. Done with the SWEEP config (`src` only) it would
    // PRUNE `extra/more.rs`; routed through `for_linked_worktree_overlay` it keeps the branch
    // target.
    let refreshed = sweep_config.for_linked_worktree_overlay(&linked);
    db.index_worktree_overlay(&refreshed, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        names_in_scope(&db, "extra/more.rs"),
        vec!["extra_fn".to_string()],
        "the main-process sweep refreshed with the branch config keeps the branch-only overlay row",
    );

    // Control: the sweep config alone (no `for_linked_worktree_overlay`) would prune it — proving
    // the bug is real and the helper is what prevents it.
    db.index_worktree_overlay(&sweep_config, &linked, &mut |_| {}).unwrap();
    assert!(
        !path_in_scope(&db, "extra/more.rs"),
        "the raw sweep config (src-only) prunes the branch-only overlay row — the bug the helper \
         fixes",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_committed_added_file_symbol_resolves_cross_connection() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    {
        // The CLI `index --worktree` writer: build the base, then overlay-index a worktree that
        // COMMITTED a brand-new file, then drop the connection.
        let mut db = IndexDatabase::rebuild(&config).unwrap();
        run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        fs::write(linked.join("src/added.rs"), "pub fn added_fn() {}\n").unwrap();
        run_git(&linked, &["add", "."]);
        run_git(&linked, &["commit", "-q", "-m", "add file"]);
        let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
        assert!(report.indexed >= 1, "the added file is indexed into the overlay");
    }

    // A FRESH connection (the MCP server querying after the CLI wrote the overlay) —
    // config-bearing, the post-A7 MCP posture (a bare open refuses the poisoned multi-repo DB).
    let mut db = IndexDatabase::open_config(&config).unwrap();
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert!(
        !db.symbols("added_fn", Some(Language::Rust), 10).unwrap().is_empty(),
        "a committed added file's symbol resolves via symbol lookup in the worktree scope \
         (cross-connection)"
    );
    // ...and is grouped into logical_symbols, so GRAPH NAV (find_callers/trace_callees resolve
    // through logical_symbols) sees it too — the overlay pass must run rebuild_logical_symbols.
    let grouped: bool = db
        .storage
        .connection()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM logical_symbols WHERE logical_name = 'added_fn')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(grouped, "the overlay's added symbol is grouped into logical_symbols (graph nav)");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_serves_worktree_version_when_main_moved_ahead() {
    // Symmetry: when MAIN advances a file the worktree branch didn't touch, the worktree scope must
    // still serve the WORKTREE's (older) version — the overlay is the worktree's view, not "newest
    // wins" (the base/worktree direction is irrelevant).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/shared.rs"), "pub fn v1() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "v1"]);
    let config = source_config(main.clone(), Language::Rust);

    // Worktree branches at v1 (it does NOT touch shared.rs afterward).
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);

    // Main moves AHEAD: shared.rs -> v2.
    fs::write(main.join("src/shared.rs"), "pub fn v2() {}\n").unwrap();
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "v2"]);

    let mut db = IndexDatabase::rebuild(&config).unwrap(); // base = main @ v2
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // Worktree scope: the worktree's v1 (overlay shadows main's newer v2).
    assert!(!db.symbols("v1", Some(Language::Rust), 10).unwrap().is_empty(), "worktree serves v1");
    assert!(
        db.symbols("v2", Some(Language::Rust), 10).unwrap().is_empty(),
        "worktree scope does not show main's newer v2"
    );
    // Base scope: main's v2.
    set_base_scope(&mut db, &main);
    assert!(!db.symbols("v2", Some(Language::Rust), 10).unwrap().is_empty(), "base serves v2");
    assert!(
        db.symbols("v1", Some(Language::Rust), 10).unwrap().is_empty(),
        "base does not show v1"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_stable_when_main_removed_a_file_a_nested_branch_keeps() {
    // Field repro of the perverse held layout: a linked worktree NESTED inside config.root and
    // gitignored there; MAIN removed a file the branch still HAS; the index retains the dead old
    // commit scope that had the file. Across repeated WATCHER maintenance passes the overlay must
    // serve that file READABLE in the worktree scope — never flip-flop to a tombstone.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    fs::write(main.join("src/reinf.rs"), "pub fn classify_seg() {}\n").unwrap();
    fs::write(main.join(".gitignore"), "/wt/\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "C1 has reinf"]);
    let config = source_config(main.clone(), Language::Rust);

    // Index at C1 → leaves a committed scope that HAS reinf.rs (the lingering dead scope).
    IndexDatabase::rebuild(&config).unwrap();

    // Linked worktree forked at C1, NESTED under main at the gitignored path.
    let linked = main.join("wt");
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);

    // Main REMOVES reinf.rs at C2; the branch keeps it on disk + in its HEAD.
    fs::remove_file(main.join("src/reinf.rs")).unwrap();
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "C2 removed reinf"]);

    for pass in 0..3 {
        crate::watch::maintenance_pass(&config, pass == 2).unwrap(); // gc on the last pass
        let mut db = IndexDatabase::open_config(&config).unwrap();
        db.use_worktree_scope(&main, Some(&linked)).unwrap();
        assert_eq!(
            names_in_scope(&db, "src/reinf.rs"),
            vec!["classify_seg".to_string()],
            "pass {pass}: worktree overlay serves the branch file (readable, not tombstoned)"
        );
    }

    let _ = fs::remove_dir_all(&main);
}

#[test]
fn worktree_overlay_keeps_base_scope_logical_grouping() {
    // #219 regression: the overlay pass's rebuild_logical_symbols must NOT de-group the
    // base (shadowed) scope. A linked worktree that MODIFIES a base file shadows the base committed
    // row; that base symbol must keep its logical handle (sym_<hex>), or graph-nav-by-id silently
    // breaks for base symbols. Before the fix the overlay rebuild ran against the worktree scope
    // view and wiped every other scope's grouping.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/shared.rs"), "pub fn shared_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Modify the base file in the worktree → the overlay shadows the base committed row.
    fs::write(linked.join("src/shared.rs"), "pub fn shared_fn() {\n    let _x = 1;\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // The BASE committed shared_fn symbol (commit_sha != '', worktree_id = '') must still be
    // grouped.
    let base_grouped: i64 = db
        .storage
        .connection()
        .query_row(
            // Query RAW main.files, not the `files` scope view: after the overlay pass the
            // connection is worktree-scoped, which SHADOWS the base committed shared.rs row.
            "SELECT COUNT(*) FROM logical_symbol_members m
             JOIN main.symbols s ON s.id = m.symbol_id
             JOIN main.files f ON f.id = s.file_id
             WHERE s.name = 'shared_fn' AND f.commit_sha != '' AND f.worktree_id = ''",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(base_grouped >= 1, "overlay pass de-grouped the base scope (graph-nav-by-id breaks)");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[cfg(unix)]
#[test]
fn worktree_overlay_resolves_through_a_symlinked_path() {
    // #219 regression: a worktree referenced via a SYMLINK must resolve to the same
    // worktree_id as the canonical path (worktree_id_of canonicalizes), so indexing via one
    // spelling and querying via another agree. Before the fix the keys diverged → silent
    // overlay miss + GC pruning the live overlay.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/added.rs"), "pub fn added_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    // Index via the CANONICAL path; query via a SYMLINK to the same checkout.
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    let symlinked = unique_temp_root();
    let _ = fs::remove_dir_all(&symlinked);
    std::os::unix::fs::symlink(&linked, &symlinked).unwrap();

    db.use_worktree_scope(&main, Some(&symlinked)).unwrap();
    assert!(
        !db.symbols("added_fn", Some(Language::Rust), 10).unwrap().is_empty(),
        "a symlinked worktree path must resolve to the same overlay as the canonical path"
    );

    let _ = fs::remove_file(&symlinked);
    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn orientation_in_a_linked_worktree_reflects_the_overlay_on_base() {
    // #219: SessionStart orientation must scope to the session's WORKTREE (the overlay on the
    // base), not the worktree's own HEAD — the index has no committed scope at a linked
    // worktree's HEAD, so the old resolve_git_context(cwd) saw only the bare overlay delta,
    // missing the base files.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let db_path = db.database_path().to_path_buf();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/added.rs"), "pub fn added_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    drop(db);

    let conn = IndexConnection::open_read_only(&db_path).unwrap();
    // Worktree cwd: overlay ON base = base.rs + keep.rs + the overlay's added.rs = 3.
    let o_wt =
        crate::query::orientation::orientation(conn.connection(), &main, &linked, None).unwrap();
    assert_eq!(
        o_wt.total_files, 3,
        "worktree orientation must show base files + the overlay's added file"
    );

    // Main cwd: base scope only = 2.
    let o_main =
        crate::query::orientation::orientation(conn.connection(), &main, &main, None).unwrap();
    assert_eq!(o_main.total_files, 2, "main orientation shows the base scope");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #219 review: when `config.root` is a SUBDIR of the repo, the tree-diff / status candidates are
/// repo-relative (`crate/src/lib.rs`) but the overlay keys + `target_for_path` are config-root-
/// relative (`src/lib.rs`). The old code filtered every subdir edit out, so the overlay was empty
/// and a worktree query kept serving the stale base. The fix rebases candidates to config-relative
/// and reads bytes from the linked checkout's equivalent of `config.root`.
#[test]
fn worktree_overlay_serves_a_subdir_rooted_config() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("crate/src")).unwrap();
    // Canonicalize the root, mirroring `Config::load`'s `normalize_existing_dir`: production
    // `config.root` is always canonical, and `linked_config_subdir_and_root` strips it against the
    // canonicalized base workdir. On macOS `std::env::temp_dir()` is `/var/folders/...` (a symlink
    // to `/private/var/folders/...`), so an un-canonicalized root fails the `strip_prefix` and the
    // subdir derivation collapses → zero overlay rows.
    let main = main.canonicalize().unwrap();
    fs::write(main.join("crate/src/lib.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    // Config rooted at the SUBDIR `crate`, indexing `crate/src`.
    let config = source_config(main.join("crate"), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Branch edits a file UNDER the config subdir.
    fs::write(linked.join("crate/src/lib.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    // The caller passes the linked worktree root; `compute_linked_worktree_delta` derives the
    // `crate` subdir and reads bytes from `<linked>/crate`.
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(report.indexed, 1, "the subdir edit must produce one overlay row");

    db.use_worktree_scope(&config.root, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/lib.rs"),
        vec!["linked_fn".to_string()],
        "the worktree query serves the branch version, keyed config-root-relative"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #219 review: a caller can pass a path INSIDE the linked checkout (e.g. `--worktree .` run from
/// `<linked>/src`) rather than its root. The overlay must still read the readable candidates from
/// the resolved workdir, not from the raw `linked_path` (which would double the `src/` prefix and
/// fail every read).
#[test]
fn worktree_overlay_accepts_a_path_inside_the_linked_checkout() {
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

    // Pass a SUBDIR of the linked checkout, not its root. `gix` discovery resolves the workdir, so
    // the readable file is read from `<linked>/src/a.rs`, not `<linked>/src/src/a.rs`.
    let inside = linked.join("src");
    let report = db.index_worktree_overlay(&config, &inside, &mut |_| {}).unwrap();
    assert_eq!(report.indexed, 1, "the readable candidate must be read from the resolved workdir");

    db.use_worktree_scope(&config.root, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["linked_fn".to_string()]);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn rebuild_restores_durable_wal_after_bulk_build() {
    // The bulk rebuild drops to journal_mode=MEMORY + synchronous=OFF for speed; it MUST
    // restore durable WAL/NORMAL afterward so later writes (reconcile, the watcher) are safe.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn alpha() {}\npub fn beta() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let journal_mode: String =
        db.storage.connection().query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
    assert_eq!(journal_mode.to_lowercase(), "wal", "rebuild must restore WAL durability");
    let synchronous: i64 =
        db.storage.connection().query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
    assert_eq!(synchronous, 1, "synchronous must be restored to NORMAL (=1)");
    // The index is intact and queryable after the bulk build.
    assert!(!db.symbols("alpha", Some(Language::Rust), 10).unwrap().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn scoped_overlay_refresh_skips_an_unlisted_worktree_with_unchanged_basis() {
    // #577: an event-scoped pass must not pay the per-worktree delta computation (tree diff +
    // status walk + ignore compile) for a worktree that is not implicated by events and whose
    // recorded refresh basis (base HEAD, linked HEAD) is unchanged. Observable proof of the skip:
    // a DIRTY edit in the unlisted worktree — visible only to a status walk — stays out of its
    // overlay on the scoped pass, and is picked up by the next `All` (periodic-sweep) pass.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_a() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn base_b() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let listed = unique_temp_root();
    let _ = fs::remove_dir_all(&listed);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-listed", listed.to_str().unwrap()]);
    let skipped = unique_temp_root();
    let _ = fs::remove_dir_all(&skipped);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-skipped", skipped.to_str().unwrap()]);

    // Full refresh records each worktree's basis (both identical to base → no overlay rows yet).
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    // DIRTY (uncommitted) edits in BOTH worktrees — no HEAD moves, so only the event scope or a
    // status walk could reveal them.
    fs::write(listed.join("src/a.rs"), "pub fn listed_dirty() {}\n").unwrap();
    fs::write(skipped.join("src/b.rs"), "pub fn skipped_dirty() {}\n").unwrap();

    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([listed.clone()]));
    let changed = crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope);
    assert!(changed, "the listed worktree's dirty edit is refreshed");

    db.use_worktree_scope(&main, Some(&listed)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["listed_dirty".to_string()],
        "the listed worktree's overlay picked up the dirty edit"
    );
    db.use_worktree_scope(&main, Some(&skipped)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/b.rs"),
        vec!["base_b".to_string()],
        "the unlisted worktree with an unchanged basis was skipped (its dirty edit is not \
         overlaid by the scoped pass)"
    );

    // The `All` (periodic-sweep) backstop heals the skipped worktree.
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    db.use_worktree_scope(&main, Some(&skipped)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/b.rs"),
        vec!["skipped_dirty".to_string()],
        "the All sweep refreshes the previously-skipped worktree"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&listed);
    let _ = fs::remove_dir_all(&skipped);
}

#[test]
fn scoped_overlay_refresh_refreshes_an_unlisted_worktree_whose_head_moved() {
    // #577: a commit in a linked worktree (a hook-driven pass, or a commit made without the
    // watcher observing file events) moves its HEAD. The recorded basis catches that: the pass
    // refreshes the worktree even when the event scope does not list it.
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
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    // Control: with an unchanged basis, an empty-scoped pass is a no-op for this worktree.
    let empty = crate::watch::OverlayScope::Linked(std::collections::BTreeSet::new());
    assert!(
        !crate::watch::refresh_worktree_overlays(&mut db, &config, None, &empty),
        "unchanged basis + empty scope refreshes nothing"
    );

    // The branch COMMITS a new file; no event scope names the worktree.
    fs::write(linked.join("src/new.rs"), "pub fn new_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    let changed = crate::watch::refresh_worktree_overlays(&mut db, &config, None, &empty);
    assert!(changed, "the moved linked HEAD invalidates the basis and forces the refresh");
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/new.rs"),
        vec!["new_fn".to_string()],
        "the committed branch file is overlaid despite the empty event scope"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn scoped_overlay_refresh_refreshes_after_a_base_commit_changes_the_diff_basis() {
    // #577: a base commit changes the base↔linked diff basis for EVERY linked worktree — files
    // the base moved past must now be shadowed by the branch's older version. The basis check
    // forces that refresh even on a pass whose event scope is empty (base-only events).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/shared.rs"), "pub fn v1() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "v1"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    set_base_scope(&mut db, &main);
    assert_eq!(overlay_row_count(&db), 0, "no divergence yet, so no overlay rows");

    // Control: empty scope + unchanged basis refreshes nothing.
    let empty = crate::watch::OverlayScope::Linked(std::collections::BTreeSet::new());
    assert!(
        !crate::watch::refresh_worktree_overlays(&mut db, &config, None, &empty),
        "unchanged basis + empty scope refreshes nothing"
    );

    // The BASE advances shared.rs to v2: the branch (still at v1) must now shadow it.
    fs::write(main.join("src/shared.rs"), "pub fn v2() {}\n").unwrap();
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "v2"]);

    let changed = crate::watch::refresh_worktree_overlays(&mut db, &config, None, &empty);
    assert!(changed, "the moved base HEAD invalidates every worktree's basis");
    set_base_scope(&mut db, &main);
    assert!(
        overlay_row_count(&db) > 0,
        "the worktree's v1 of the file the base moved past is now overlaid"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn worktree_overlay_gc_prunes_a_removed_worktrees_refresh_basis() {
    // #577: the refresh-basis marker (`repo_meta` `worktree_overlay_basis:<id>`) must follow the
    // overlay rows: gc drops a removed worktree's key and keeps a live sibling's.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let removed = unique_temp_root();
    let _ = fs::remove_dir_all(&removed);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-removed", removed.to_str().unwrap()]);
    let kept = unique_temp_root();
    let _ = fs::remove_dir_all(&kept);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-kept", kept.to_str().unwrap()]);

    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    let removed_id = crate::index::worktree_id_of(&removed);
    let kept_id = crate::index::worktree_id_of(&kept);
    assert!(
        db.worktree_overlay_basis(&removed_id).unwrap().is_some(),
        "the refresh recorded a basis for each worktree"
    );

    set_base_scope(&mut db, &main);
    run_git(&main, &["worktree", "remove", "--force", removed.to_str().unwrap()]);
    db.garbage_collect().unwrap();
    assert_eq!(
        db.worktree_overlay_basis(&removed_id).unwrap(),
        None,
        "gc prunes the removed worktree's refresh basis with its overlay"
    );
    assert!(
        db.worktree_overlay_basis(&kept_id).unwrap().is_some(),
        "a live worktree's basis survives gc"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&removed);
    let _ = fs::remove_dir_all(&kept);
}

/// Whether the repo's `logical_symbols` grouping has a row named `name` — the table
/// symbol_lookup / graph nav resolve through, i.e. the direct observable for "the repo-global
/// rebuild ran (or didn't) since these symbols were indexed".
fn logical_symbol_named(db: &IndexDatabase, name: &str) -> bool {
    db.storage
        .connection()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main.logical_symbols WHERE repo_id = ?1 AND logical_name \
             = ?2)",
            rusqlite::params![db.active_repo_id, name],
            |row| row.get(0),
        )
        .unwrap()
}

/// The #819 pending-rebuild marker, so tests can assert a batch commits it with the overlay rows
/// and the batch tail consumes it.
fn logical_rebuild_pending(db: &IndexDatabase) -> bool {
    db.repo_meta("overlay_logical_rebuild_pending").unwrap().is_some()
}

#[test]
fn overlay_batch_pass_rebuilds_logical_symbols_once() {
    // #819: `logical_symbols` is repo-scoped but scope-INDEPENDENT, so a pass refreshing K
    // changed worktrees needs exactly ONE repo-global rebuild — per-worktree inline rebuilds
    // are K−1 redundant DELETE-all + re-derive passes (only the last one's output survives).
    // The counter is the cardinality observable: the old inline behavior paid one rebuild per
    // changed worktree and fails this assertion.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let wt_one = unique_temp_root();
    let _ = fs::remove_dir_all(&wt_one);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-one", wt_one.to_str().unwrap()]);
    fs::write(wt_one.join("src/one.rs"), "pub fn one_fn() {}\n").unwrap();
    run_git(&wt_one, &["add", "."]);
    run_git(&wt_one, &["commit", "-q", "-m", "one"]);
    let wt_two = unique_temp_root();
    let _ = fs::remove_dir_all(&wt_two);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-two", wt_two.to_str().unwrap()]);
    fs::write(wt_two.join("src/two.rs"), "pub fn two_fn() {}\n").unwrap();
    run_git(&wt_two, &["add", "."]);
    run_git(&wt_two, &["commit", "-q", "-m", "two"]);

    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    let after = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(after - before, 1, "two changed worktrees, ONE repo-global rebuild");
    assert!(!logical_rebuild_pending(&db), "the batch tail consumed the pending marker");
    // The deferred rebuild still lands every branch's new symbols in the grouping — the #219
    // field bug (a newly added overlay file's symbols unresolvable) stays fixed.
    assert!(logical_symbol_named(&db, "one_fn"), "first worktree's new symbol is grouped");
    assert!(logical_symbol_named(&db, "two_fn"), "second worktree's new symbol is grouped");

    // An idle follow-up sweep neither rebuilds nor marks anything pending (#63 idle backstop).
    let idle_before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        idle_before,
        "an unchanged fleet rebuilds nothing"
    );
    assert!(!logical_rebuild_pending(&db));

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&wt_one);
    let _ = fs::remove_dir_all(&wt_two);
}

#[test]
fn pending_logical_rebuild_marker_heals_an_interrupted_batch() {
    // #819 crash-window backstop: a Deferred overlay refresh commits its rows and the pending
    // marker in ONE transaction; if the process dies before the batch tail, the next pass
    // idle-skips the (now unchanged) overlay rows — only the persisted marker can force the
    // owed rebuild then. Without it, a newly added branch file's symbols would stay
    // unresolvable until an unrelated change triggered a rebuild.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Simulate the interrupted batch: the refresh commits (rows + marker); the tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    let report = db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the branch file landed as an overlay row");
    assert!(logical_rebuild_pending(&db), "the obligation is committed with the rows");
    assert!(
        !logical_symbol_named(&db, "branch_only_fn"),
        "committed rows without the batch tail leave the grouping stale — the state the persisted \
         marker exists to heal"
    );

    // The next maintenance pass finds every overlay row unchanged (identity-skip) but still
    // runs the owed rebuild off the marker.
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        1,
        "the write-idle pass still runs the owed rebuild"
    );
    assert!(!logical_rebuild_pending(&db), "the healing pass consumed the marker");
    assert!(logical_symbol_named(&db, "branch_only_fn"), "the branch symbol is now grouped");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn inline_rebuild_satisfies_a_pending_deferred_obligation() {
    // #819 review: a stale pending marker (left by an interrupted deferred batch) must be
    // consumed by ANY successful rebuild — here a standalone INLINE overlay refresh — not only
    // by the batch tail. Left set, the next maintenance pass would pay a second wholesale
    // rebuild the inline one already performed.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Interrupted deferred batch: rows + marker committed, the tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");

    // A later dirty edit in the same worktree, refreshed via the STANDALONE (inline) route.
    fs::write(linked.join("src/more.rs"), "pub fn later_fn() {}\n").unwrap();
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the dirty edit landed as an overlay row");
    assert!(!logical_rebuild_pending(&db), "the inline rebuild consumed the stale obligation");
    // The rebuild is repo-global, so it grouped the interrupted batch's earlier symbols too.
    assert!(logical_symbol_named(&db, "branch_only_fn"));
    assert!(logical_symbol_named(&db, "later_fn"));

    // The next sweep owes nothing — no second wholesale rebuild.
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "an already-satisfied obligation triggers nothing"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn standalone_unchanged_refresh_settles_a_pending_deferred_obligation() {
    // #819 review P2: an interrupted Deferred batch leaves the pending marker committed. A
    // standalone `index --worktree` (Inline) over the UNCHANGED checkout then has an EMPTY
    // delta — `finalize_overlay_refresh` (and with it the inline rebuild, the sole marker
    // clearer) is skipped entirely — so without the entry-point tail settle the run would
    // exit leaving BOTH the marker and the stale `logical_symbols` in place: branch-only
    // symbols invisible to symbol/graph queries until some unrelated pass consumed the
    // marker. The invariant: EVERY indexing entry point settles a pending obligation before
    // exiting, even when its own delta is empty.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Interrupted deferred batch: rows + marker committed, the batch tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");
    assert!(!logical_symbol_named(&db, "branch_only_fn"), "the grouping is stale");

    // The standalone (Inline) refresh over the SAME, unchanged worktree: zero row changes.
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(report.indexed, 0, "unchanged checkout: the refresh itself writes no rows");
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        1,
        "the entry-point tail ran the ONE owed rebuild"
    );
    assert!(!logical_rebuild_pending(&db), "the standalone exit consumed the obligation");
    assert!(logical_symbol_named(&db, "branch_only_fn"), "branch symbols are grouped again");

    // Settled means settled: a second unchanged standalone run owes (and pays) nothing.
    let idle = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        idle,
        "no obligation pending: the tail settle is a single meta read"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn idle_base_incremental_pass_settles_a_pending_deferred_obligation() {
    // Same class as the standalone-worktree exit (#819 review): the base incremental writer's
    // logical rebuild is gated on its OWN row changes (indexed/healed/carried/roots_changed),
    // so an IDLE `rag-rat index` pass over an unchanged base tree closes its empty transaction
    // and would exit past a committed obligation. The pass settles at its tail instead — one
    // meta read when nothing is pending.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Interrupted deferred batch: rows + marker committed, the batch tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");

    // An idle base incremental pass (its own connection, like the CLI `index`): the base tree
    // is unchanged, so the pass's gated rebuild never fires — only the tail settle can.
    let refreshed = IndexDatabase::index_changed(&config).unwrap();
    drop(refreshed);
    assert!(!logical_rebuild_pending(&db), "the idle incremental pass settled the obligation");
    assert!(logical_symbol_named(&db, "branch_only_fn"), "branch symbols are grouped again");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn path_scoped_inline_refresh_settles_a_pending_deferred_obligation() {
    // The path-scoped overlay twin (#679) has the same empty-delta exit as the whole-delta
    // route: an unchanged supplied path identity-skips, `finalize_overlay_refresh` never runs,
    // and a stale pending obligation (#819 review) must still be settled at the entry point's
    // tail. The non-sibling no-op exit is an entry-point exit too.
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
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);

    // Interrupted deferred batch: rows + marker committed, the batch tail never runs.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");

    // Path-scoped INLINE refresh of the UNCHANGED path: identity-skip, zero row changes.
    let report = db
        .index_worktree_overlay_paths(
            &config,
            &linked,
            &[linked.join("src/new.rs")],
            crate::index::OverlayLogicalRebuild::Inline,
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(report.indexed, 0, "unchanged path: the refresh itself writes no rows");
    assert!(!logical_rebuild_pending(&db), "the path-scoped exit consumed the obligation");
    assert!(logical_symbol_named(&db, "branch_only_fn"), "branch symbols are grouped again");

    // Re-arm the obligation with a second interrupted Deferred refresh...
    fs::write(linked.join("src/more.rs"), "pub fn second_branch_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "second branch file"]);
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the second interrupted batch re-armed it");
    // ...then exit through the NON-SIBLING no-op arm (the base root is not a linked sibling):
    // still an Inline entry-point exit, so it settles too.
    let report = db.index_worktree_overlay(&config, &main, &mut |_| {}).unwrap();
    assert!(report.worktree_id.is_empty(), "base root refresh is the no-op arm");
    assert!(!logical_rebuild_pending(&db), "every Inline entry-point exit settles");
    assert!(logical_symbol_named(&db, "second_branch_fn"), "the settle grouped the new symbol");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_basis_writes_ride_the_refresh_transaction() {
    // #824 (#577 semantics preserved): the skip-proof basis is maintained INSIDE the overlay's
    // own BEGIN IMMEDIATE — record the caller's pair on a COMPLETE refresh (even a no-change
    // one: the proof "this pair is current" is the point), clear it on a PARTIAL one, leave it
    // untouched when no basis is maintained — instead of a separate autocommit per worktree
    // per pass.
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

    // COMPLETE refresh with a maintained basis: the CALLER's pair (read around the refresh,
    // like the watcher's) is recorded in the refresh transaction.
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: Some(crate::index::OverlayBasisUpdate {
            base_sha: "base-head-1",
            linked_head_sha: "linked-head-1",
        }),
    };
    let report = db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(report.status_complete, "an undisturbed delta walk completes");
    assert_eq!(
        db.worktree_overlay_basis(&report.worktree_id).unwrap(),
        Some(("base-head-1".to_string(), "linked-head-1".to_string())),
        "a complete refresh records the maintained basis"
    );
    assert!(!logical_rebuild_pending(&db), "a no-change refresh defers no rebuild");

    // PARTIAL refresh clears the recorded proof: a dirty edit moves no HEAD, so a stale pair
    // would keep matching and scoped passes would skip the stale overlay until an `All` sweep
    // (#577 review). Exercised at the tail seam — a real mid-walk gix status failure can't be
    // provoked deterministically.
    db.apply_overlay_basis_tail(
        &report.worktree_id,
        false,
        Some(crate::index::OverlayBasisUpdate {
            base_sha: "base-head-2",
            linked_head_sha: "linked-head-2",
        }),
    )
    .unwrap();
    assert_eq!(
        db.worktree_overlay_basis(&report.worktree_id).unwrap(),
        None,
        "a partial refresh drops the stale skip proof"
    );

    // The standalone shape maintains no basis: whatever is recorded stays untouched.
    db.apply_overlay_basis_tail(
        &report.worktree_id,
        true,
        Some(crate::index::OverlayBasisUpdate {
            base_sha: "base-head-3",
            linked_head_sha: "linked-head-3",
        }),
    )
    .unwrap();
    let standalone = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.worktree_overlay_basis(&standalone.worktree_id).unwrap(),
        Some(("base-head-3".to_string(), "linked-head-3".to_string())),
        "a refresh that maintains no basis leaves the recorded pair alone"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// How many `logical_symbol_members` rows for `logical_name`'s logical rows resolve to a LIVE
/// symbol row — the #820 relink observable: a member pointing at a deleted symbol id (or a
/// replacement symbol missing its member row) shows up here as a wrong count, which is exactly
/// the breakage a false key-stable verdict would cause.
fn live_member_count(db: &IndexDatabase, logical_name: &str) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.logical_symbol_members m
             JOIN main.logical_symbols ls ON ls.id = m.logical_symbol_id
             JOIN main.symbols s ON s.id = m.symbol_id
             WHERE ls.repo_id = ?1 AND ls.logical_name = ?2",
            rusqlite::params![db.active_repo_id, logical_name],
            |row| row.get(0),
        )
        .unwrap()
}

/// Every grouped row + member of the repo, rendered to comparable strings — so a test can assert
/// the #820 relink path's output is the FIXED POINT of `rebuild_logical_symbols` (running the
/// wholesale rebuild on top of a relinked state changes nothing, byte for byte).
fn logical_grouping_snapshot(db: &IndexDatabase) -> Vec<String> {
    let conn = db.storage.connection();
    let mut snapshot = Vec::new();
    let mut rows = conn
        .prepare(
            "SELECT ls.id, ls.language, ls.path, ls.logical_name,
                    COALESCE((SELECT value FROM name_strings WHERE id = ls.qualified_name_id), ''),
                    ls.kind, ls.variant_count, ls.group_reason
             FROM main.logical_symbols ls WHERE ls.repo_id = ?1 ORDER BY ls.id",
        )
        .unwrap()
        .query_map(rusqlite::params![db.active_repo_id], |row| {
            Ok(format!(
                "row {}|{}|{}|{}|{}|{}|{}|{}",
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    snapshot.append(&mut rows);
    let mut members = conn
        .prepare(
            "SELECT m.logical_symbol_id, m.symbol_id, COALESCE(m.cfg_expr, ''),
                    COALESCE(m.signature_hash, ''), m.start_line, m.end_line
             FROM main.logical_symbol_members m
             JOIN main.logical_symbols ls ON ls.id = m.logical_symbol_id
             WHERE ls.repo_id = ?1 ORDER BY m.logical_symbol_id, m.symbol_id",
        )
        .unwrap()
        .query_map(rusqlite::params![db.active_repo_id], |row| {
            Ok(format!(
                "member {}|{}|{}|{}|{}|{}",
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    snapshot.append(&mut members);
    snapshot
}

#[test]
fn a_body_only_overlay_edit_relinks_members_without_a_rebuild() {
    // #820: a body-only edit re-inserts the file's symbols under NEW row ids, but the logical
    // KEY multiset (language, path, name, qualified name, kind, signature) is unchanged — the
    // rebuilt `logical_symbols` table would be identical, so the refresh re-points the members
    // at the replacement symbol ids instead of paying the whole-repo DELETE-all + re-derive.
    // The counter is the observable: pre-#820 this pass rebuilt (delta 1) and fails the zero
    // assertion below.
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
    // MULTI-line fn: the stored signature is the declaration's first line, so a body edit below
    // it keeps all six key columns. (A one-liner's body is part of its signature line — that
    // edit is a key change and correctly keeps the rebuild.)
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    1\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);

    // First refresh: the overlay file is ADDED — a key-set change, so the batch pays its one
    // rebuild and the branch symbol becomes resolvable.
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert!(logical_symbol_named(&db, "one_fn"), "the added overlay file is grouped");

    // Body-only edit: same declaration line, different body — the key multiset holds.
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    2\n}\n").unwrap();
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        0,
        "a key-stable batch pays ZERO repo-global rebuilds"
    );
    assert!(!logical_rebuild_pending(&db), "a key-stable batch creates no rebuild obligation");
    // The replacement symbols are grouped and their members resolve to LIVE symbol rows — the
    // exact property a false key-stable verdict would break.
    assert!(logical_symbol_named(&db, "one_fn"));
    assert_eq!(live_member_count(&db, "one_fn"), 1, "the member points at the live symbol");

    // The relinked state is the wholesale rebuild's FIXED POINT: forcing the rebuild changes
    // nothing, byte for byte — grouped rows, ids, variant counts, member spans and hashes.
    let relinked = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(
        logical_grouping_snapshot(&db),
        relinked,
        "relink output must equal the rebuild's output"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn key_altering_edits_force_the_full_rebuild() {
    // #820 adversarial matrix: every change that alters a file's logical-key multiset must keep
    // today's rebuild — a signature change, a rename, an added file, a tombstoned base file,
    // and a pruned branch-only file. A false key-stable verdict on any of these would leave
    // members pointing at deleted symbol ids or stale rows orphaned in the grouping.
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
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    1\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert!(logical_symbol_named(&db, "one_fn"));
    let rebuilds =
        |db: &IndexDatabase| db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);

    // A SIGNATURE change (same name) alters the key.
    fs::write(linked.join("src/one.rs"), "pub fn one_fn(bump: i32) -> i32 {\n    bump\n}\n")
        .unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "a signature change pays the rebuild");
    assert_eq!(live_member_count(&db, "one_fn"), 1, "the re-derived group is live");

    // A RENAME drops one key and adds another; the stale logical row must be GONE afterwards —
    // exactly the orphan a falsely-engaged relink would leave behind.
    fs::write(linked.join("src/one.rs"), "pub fn one_fn_renamed() -> i32 {\n    3\n}\n").unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "a rename pays the rebuild");
    assert!(logical_symbol_named(&db, "one_fn_renamed"));
    assert!(!logical_symbol_named(&db, "one_fn"), "the old key's row is dropped, not orphaned");

    // An ADDED file introduces new keys.
    fs::write(linked.join("src/two.rs"), "pub fn two_fn() -> i32 {\n    4\n}\n").unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "an added file pays the rebuild");
    assert!(logical_symbol_named(&db, "two_fn"));

    // A TOMBSTONED base file (deleted in the worktree, present in the base tree) removes keys
    // from the branch's view.
    fs::remove_file(linked.join("src/a.rs")).unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "a tombstoned file pays the rebuild");

    // A PRUNED branch-only file (deleted from the worktree with no base row to shadow) removes
    // its keys entirely.
    fs::remove_file(linked.join("src/one.rs")).unwrap();
    let before = rebuilds(&db);
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert_eq!(rebuilds(&db) - before, 1, "a pruned overlay row pays the rebuild");
    assert!(!logical_symbol_named(&db, "one_fn_renamed"), "the pruned file's keys are dropped");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_key_stable_relink_leaves_a_pending_obligation_for_the_tail() {
    // #820 × #819: the relink path must NEVER clear (or mask) a pre-existing pending rebuild
    // obligation — only `rebuild_logical_symbols` clears the marker. An interrupted Deferred
    // batch leaves the marker committed and an UNGROUPED overlay file behind; a body-only
    // follow-up edit then takes the relink path (its own file's grouping is intact), and the
    // marker must survive that refresh for the batch tail to settle.
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
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    1\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);
    // A complete first refresh grounds one_fn's grouping.
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert!(logical_symbol_named(&db, "one_fn"));

    // Interrupted Deferred batch (#819's scenario): a NEW branch file's rows + the marker are
    // committed, the batch tail never runs — its symbols stay ungrouped.
    fs::write(linked.join("src/new.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    let tail = crate::index::OverlayRefreshTail {
        logical_rebuild: crate::index::OverlayLogicalRebuild::Deferred,
        basis: None,
    };
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert!(logical_rebuild_pending(&db), "the interrupted batch left its obligation");
    assert!(!logical_symbol_named(&db, "branch_only_fn"), "the grouping is stale");

    // Body-only follow-up edit, refreshed Deferred WITHOUT a tail (the next interrupted-batch
    // window): the edited file is key-stable, so the refresh takes the relink path — and the
    // outstanding obligation must be untouched.
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    2\n}\n").unwrap();
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "the key-stable refresh itself rebuilds nothing"
    );
    assert!(
        logical_rebuild_pending(&db),
        "the relink path must not clear a pre-existing obligation"
    );
    assert_eq!(live_member_count(&db, "one_fn"), 1, "the relink still landed in the refresh txn");

    // The batch tail settles the surviving obligation: ONE rebuild, marker consumed, the
    // interrupted file's symbols finally grouped.
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    assert!(db.apply_pending_logical_rebuild().unwrap(), "the tail found the obligation");
    assert_eq!(db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before, 1);
    assert!(!logical_rebuild_pending(&db));
    assert!(logical_symbol_named(&db, "branch_only_fn"), "the settle grouped the stale file");
    assert_eq!(live_member_count(&db, "one_fn"), 1, "the rebuild agrees with the relink");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_body_only_base_edit_pass_relinks_without_a_rebuild() {
    // #820 on the BASE incremental writer: the second body-only edit of an already-dirty file
    // replaces the same scope row under an identical key multiset, so the pass's tail re-links
    // members instead of rebuilding. (The FIRST dirty edit moves the file into the worktree scope —
    // a key-set change for that scope — which #826 now serves with a PATH-SCOPED re-derive rather
    // than the whole-repo rebuild; either way the second edit relinks, and both outputs are the
    // wholesale rebuild's fixed point.)
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() -> i32 {\n    1\n}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    // First dirty edit: the file enters the worktree overlay scope (an ADD there — a key-set
    // change). #826 serves it with the path-scoped re-derive, so the pass runs ZERO whole-repo
    // rebuilds; the resulting grouping is still correct (the relink fixed point below builds on
    // it).
    fs::write(main.join("src/a.rs"), "pub fn base_fn() -> i32 {\n    2\n}\n").unwrap();
    let first = IndexDatabase::index_paths(&config, &[main.join("src/a.rs")]).unwrap();
    assert_eq!(
        first.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "#826: the scope move is a key-set change served by the path-scoped re-derive, not a \
         whole-repo rebuild"
    );
    assert!(
        logical_symbol_named(&first, "base_fn"),
        "the scoped re-derive grouped the overlay add"
    );
    drop(first);

    // Second body-only edit: same scope row replaced, identical key multiset — zero rebuilds
    // on the whole pass (pre-#820 this was 1).
    fs::write(main.join("src/a.rs"), "pub fn base_fn() -> i32 {\n    3\n}\n").unwrap();
    let second = IndexDatabase::index_paths(&config, &[main.join("src/a.rs")]).unwrap();
    assert_eq!(
        second.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a key-stable base pass pays ZERO repo-global rebuilds"
    );
    assert!(logical_symbol_named(&second, "base_fn"));
    // Committed row + dirty overlay row both carry the key → two live members, the overlay one
    // pointing at the replacement symbol id.
    assert_eq!(live_member_count(&second, "base_fn"), 2);

    // Fixed-point check on the base route too.
    let relinked = logical_grouping_snapshot(&second);
    second.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(logical_grouping_snapshot(&second), relinked);

    let _ = fs::remove_dir_all(&main);
}

/// One committed base repo + one linked worktree, the shared #822/#825 shape: `(main, linked,
/// config, db)` with `src/a.rs` committed on both sides and every overlay basis unrecorded.
fn quiet_window_fixture() -> (PathBuf, PathBuf, Config, IndexDatabase) {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_a() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    (main, linked, config, db)
}

#[test]
fn overlay_quiet_window_skips_a_dirty_only_listed_worktree_inside_the_window() {
    // #822: dirty working-tree churn fires events that LIST the worktree, and pre-#822 every such
    // pass re-paid the full per-worktree probe (tree diff + status walk + ignore compile). While
    // both heads still equal the recorded basis AND the last complete refresh is younger than the
    // quiet window, the scoped pass skips the worktree outright; the window elapsing re-arms the
    // refresh.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 300;

    // The All pass records the basis WITH its fresh timestamp (the window anchor).
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    // Dirty-only churn: no HEAD moves, the event scope lists the worktree.
    fs::write(linked.join("src/a.rs"), "pub fn dirty_fn() {}\n").unwrap();
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        !crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "inside the window a dirty-only listed worktree is skipped"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["base_a".to_string()],
        "the dirty edit stays un-overlaid while the quiet window holds"
    );

    // The window elapsing re-arms the refresh: backdate the recorded timestamp past the window.
    set_base_scope(&mut db, &main);
    let worktree_id = crate::index::worktree_id_of(&linked);
    let (base_sha, linked_head) = db.worktree_overlay_basis(&worktree_id).unwrap().unwrap();
    db.record_worktree_overlay_basis(
        &worktree_id,
        &base_sha,
        &linked_head,
        rag_rat_base::time::now_ms() - 301_000,
    )
    .unwrap();
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "an elapsed window refreshes the dirty-only worktree"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["dirty_fn".to_string()],
        "the deferred dirty edit lands once the window elapses"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_quiet_window_never_defers_a_head_move() {
    // #822: committed freshness stays immediate — a commit moves a HEAD, the recorded basis
    // mismatches, and the refresh runs no matter how young the window is. Both directions: a
    // linked commit, then a base commit.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 300;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    // Linked commit, seconds into the window, on a pass that lists the worktree.
    fs::write(linked.join("src/new.rs"), "pub fn committed_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds file"]);
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "a linked HEAD move refreshes immediately inside the window"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/new.rs"), vec!["committed_fn".to_string()]);

    // Base commit: every worktree's diff basis moves; even an EMPTY event scope refreshes.
    fs::write(main.join("src/a.rs"), "pub fn base_v2() {}\n").unwrap();
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base v2"]);
    set_base_scope(&mut db, &main);
    let empty = crate::watch::OverlayScope::Linked(std::collections::BTreeSet::new());
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &empty),
        "a base HEAD move refreshes immediately inside the window"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_quiet_window_zero_disables_and_sweeps_ignore_it() {
    // #822: `overlay_quiet_secs = 0` restores per-pass refresh for listed worktrees, and an `All`
    // sweep never consults the window even when it is armed — the sweep is one of the backstops
    // the window's staleness bound leans on.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 0;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    fs::write(linked.join("src/a.rs"), "pub fn dirty_v1() {}\n").unwrap();
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "0 disables the window: a dirty-only listed worktree refreshes every pass"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["dirty_v1".to_string()]);

    // Arm the window; the All sweep refreshes the fresh dirty edit regardless.
    set_base_scope(&mut db, &main);
    config.watch.overlay_quiet_secs = 300;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    fs::write(linked.join("src/a.rs"), "pub fn dirty_v2() {}\n").unwrap();
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["dirty_v2".to_string()],
        "an All sweep is never held back by the quiet window"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_quiet_window_is_ignored_when_the_periodic_sweep_is_disabled() {
    // #822: the window defers work it never schedules — a deferred edit is delivered by the next
    // post-window event pass or by the periodic sweep. With `periodic_sweep_secs = 0` a one-off
    // dirty edit skipped inside the window could have neither (no follow-up event, no sweep) and
    // stay invisible indefinitely, so the gate must disable itself.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 300;
    config.watch.periodic_sweep_secs = 0;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    fs::write(linked.join("src/a.rs"), "pub fn dirty_fn() {}\n").unwrap();
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "no sweep backstop → the window is ignored and the pass refreshes"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["dirty_fn".to_string()]);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_cleared_basis_never_quiet_skips() {
    // #822: the timestamp rides the basis value, so the #577 clear paths (partial refresh in-txn,
    // failed refresh caller-side) drop the window anchor with the pair — after a clear, a scoped
    // pass must refresh even though the window would otherwise still hold.
    let (main, linked, mut config, mut db) = quiet_window_fixture();
    config.watch.overlay_quiet_secs = 300;
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    fs::write(linked.join("src/a.rs"), "pub fn dirty_fn() {}\n").unwrap();
    // The failure path's clear (the watcher's Err arm calls exactly this).
    let worktree_id = crate::index::worktree_id_of(&linked);
    db.clear_worktree_overlay_basis(&worktree_id).unwrap();
    let scope =
        crate::watch::OverlayScope::Linked(std::collections::BTreeSet::from([linked.clone()]));
    assert!(
        crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope),
        "no recorded basis, no quiet skip — the pass refreshes"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["dirty_fn".to_string()]);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_basis_value_parses_stamped_and_legacy_shapes() {
    // #822: the basis meta value grew a third line (the last-complete-refresh timestamp). A
    // pre-#822 two-line value must still yield the #577 pair — only the quiet window stays
    // disarmed — and a malformed timestamp degrades the same way instead of poisoning the pair.
    let (main, linked, _config, db) = quiet_window_fixture();

    db.record_worktree_overlay_basis("wt-stamped", "base-1", "linked-1", 42).unwrap();
    assert_eq!(
        db.worktree_overlay_basis("wt-stamped").unwrap(),
        Some(("base-1".to_string(), "linked-1".to_string()))
    );
    assert_eq!(db.worktree_overlay_basis_refreshed_at_ms("wt-stamped").unwrap(), Some(42));

    // A legacy value written by a pre-#822 build.
    db.set_repo_meta_if_changed("worktree_overlay_basis:wt-legacy", "base-2\nlinked-2").unwrap();
    assert_eq!(
        db.worktree_overlay_basis("wt-legacy").unwrap(),
        Some(("base-2".to_string(), "linked-2".to_string())),
        "the pair survives a pre-#822 value"
    );
    assert_eq!(
        db.worktree_overlay_basis_refreshed_at_ms("wt-legacy").unwrap(),
        None,
        "no timestamp on a legacy value — the quiet window never holds"
    );

    db.set_repo_meta_if_changed("worktree_overlay_basis:wt-garbage", "base-3\nlinked-3\nnot-ms")
        .unwrap();
    assert_eq!(
        db.worktree_overlay_basis("wt-garbage").unwrap(),
        Some(("base-3".to_string(), "linked-3".to_string()))
    );
    assert_eq!(db.worktree_overlay_basis_refreshed_at_ms("wt-garbage").unwrap(), None);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// The overlay's `(path, is_tombstone)` rows for `worktree_id` at the live scope — the observable
/// the #825 tree-diff-skip equivalence is asserted on (tombstones included: pruning one would
/// un-shadow a branch-deleted base file).
fn overlay_rows(db: &IndexDatabase, worktree_id: &str) -> Vec<(String, bool)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT path, kind = 'deleted' FROM main.files WHERE worktree_id = ?1 AND commit_sha \
             = '' ORDER BY path",
        )
        .unwrap();
    let rows = stmt
        .query_map([worktree_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)))
        .unwrap();
    rows.filter_map(Result::ok).collect()
}

#[test]
fn heads_unchanged_refresh_reuses_the_committed_delta_and_still_sees_dirty_edits() {
    // #825: when both heads equal the recorded basis, the refresh seeds its committed candidates
    // from the current overlay rows instead of walking the base↔linked tree diff; the status walk
    // still runs. Equivalence on a heads-unchanged fixture: the committed rows (a modified file's
    // overlay row, a deleted file's tombstone) come out identical, and a fresh dirty edit is
    // still picked up. The PROOF the tree walk was actually skipped is the seed path's one
    // accepted divergence: a previously-dirty file REVERTED to base content keeps its
    // (content-identical) overlay row — the full diff would have pruned it — until the next
    // HEAD move runs the full diff and does.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_a() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn base_b() {}\n").unwrap();
    fs::write(main.join("src/c.rs"), "pub fn base_c() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Committed branch delta: modify a.rs, delete b.rs (→ tombstone).
    fs::write(linked.join("src/a.rs"), "pub fn branch_a() {}\n").unwrap();
    fs::remove_file(linked.join("src/b.rs")).unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch delta"]);
    // Dirty edit that will be REVERTED before the seeded pass.
    fs::write(linked.join("src/c.rs"), "pub fn dirty_c() {}\n").unwrap();

    // Refresh maintaining the basis with REAL heads, exactly as the watcher does.
    let refresh = |db: &mut IndexDatabase| {
        let base_sha = crate::index::head_sha(&main);
        let linked_head = crate::index::head_sha(&linked);
        let tail = crate::index::OverlayRefreshTail {
            logical_rebuild: crate::index::OverlayLogicalRebuild::Inline,
            basis: Some(crate::index::OverlayBasisUpdate {
                base_sha: &base_sha,
                linked_head_sha: &linked_head,
            }),
        };
        db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap()
    };
    // Pass 1: no recorded basis → the full tree diff.
    let first = refresh(&mut db);
    assert!(first.status_complete);
    assert_eq!(first.indexed, 2, "branch a.rs + dirty c.rs");
    assert_eq!(first.tombstoned, 1, "deleted b.rs shadows its base row");
    let worktree_id = first.worktree_id.clone();
    let committed_rows = overlay_rows(&db, &worktree_id);

    // Heads unchanged; revert c.rs to base content and add a NEW dirty file d.rs.
    fs::write(linked.join("src/c.rs"), "pub fn base_c() {}\n").unwrap();
    fs::write(linked.join("src/d.rs"), "pub fn dirty_d() {}\n").unwrap();
    let second = refresh(&mut db);
    assert!(second.status_complete);
    let expected = {
        let mut expected = committed_rows.clone();
        expected.push(("src/d.rs".to_string(), false));
        expected.sort();
        expected
    };
    assert_eq!(
        overlay_rows(&db, &worktree_id),
        expected,
        "the seeded pass reproduces the committed delta exactly (a.rs row, b.rs tombstone), still \
         catches the new dirty d.rs via the status walk, and keeps the reverted c.rs row — the \
         divergence that proves the tree diff was skipped (a full diff prunes it)"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["branch_a".to_string()]);
    assert!(!path_in_scope(&db, "src/b.rs"), "the tombstone still shadows the base row");
    assert_eq!(names_in_scope(&db, "src/d.rs"), vec!["dirty_d".to_string()]);

    // A HEAD move runs the full diff again, which prunes the lingering reverted row.
    set_base_scope(&mut db, &main);
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "commit the dirty work"]);
    let third = refresh(&mut db);
    assert!(third.status_complete);
    assert!(
        !overlay_rows(&db, &worktree_id).iter().any(|(path, _)| path == "src/c.rs"),
        "the next tree-diff refresh prunes the content-identical reverted row"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_dirty_gitignore_edit_forces_the_full_diff_on_matching_heads() {
    // #825: the one committed-side case the row seed provably cannot see — an ignore flip making
    // a committed branch-only file indexable for the FIRST time (it produced no row, and it is
    // tracked + clean so it is not in status). A dirty `.gitignore` among the status candidates
    // must force the full tree diff even though the heads match the basis.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_a() {}\n").unwrap();
    fs::write(main.join("src/.gitignore"), "gen.rs\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch COMMITS an ignored file (forced add): a tree-diff candidate on the first pass,
    // but not indexable → no overlay row.
    fs::write(linked.join("src/gen.rs"), "pub fn generated_fn() {}\n").unwrap();
    run_git(&linked, &["add", "-f", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds ignored file"]);

    let refresh = |db: &mut IndexDatabase| {
        let base_sha = crate::index::head_sha(&main);
        let linked_head = crate::index::head_sha(&linked);
        let tail = crate::index::OverlayRefreshTail {
            logical_rebuild: crate::index::OverlayLogicalRebuild::Inline,
            basis: Some(crate::index::OverlayBasisUpdate {
                base_sha: &base_sha,
                linked_head_sha: &linked_head,
            }),
        };
        db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap()
    };
    let first = refresh(&mut db);
    assert!(first.status_complete);
    assert_eq!(first.indexed, 0, "the committed file is ignored — no overlay row yet");

    // Heads unchanged; a DIRTY `.gitignore` edit unignores it. Without the forced full diff the
    // seeded pass could never surface gen.rs: it has no row and is absent from status.
    fs::write(linked.join("src/.gitignore"), "").unwrap();
    let second = refresh(&mut db);
    assert!(second.status_complete);
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/gen.rs"),
        vec!["generated_fn".to_string()],
        "the dirty .gitignore edit forced the tree diff, surfacing the newly-unignored file"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #826: adding a function changes a file's logical-key set (a non-#820-relink change) — which used
/// to pay the whole-repo `rebuild_logical_symbols` on every incremental pass. It now re-derives
/// ONLY the changed path's groups. Observable: the base incremental pass runs ZERO whole-repo
/// logical rebuilds, yet the grouping is the wholesale rebuild's FIXED POINT (byte-identical rows,
/// ids, variant counts, member spans and hashes).
#[test]
fn a_key_set_change_scopes_the_logical_rederive_to_changed_paths() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn b_one() {}\npub fn b_two() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    drop(IndexDatabase::rebuild(&config).unwrap());

    // Add a function to a.rs (dirty, uncommitted) — a key-set change routes to the rebuild branch.
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\npub fn a_two() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();

    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a scoped re-derive runs ZERO whole-repo logical rebuilds"
    );
    assert!(logical_symbol_named(&db, "a_two"), "the added function is grouped");
    assert!(logical_symbol_named(&db, "b_one"), "the unchanged file's grouping is intact");

    // The scoped output is the wholesale rebuild's FIXED POINT — forcing the rebuild changes
    // nothing.
    let scoped = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(
        logical_grouping_snapshot(&db),
        scoped,
        "the scoped re-derive output must equal the whole-repo rebuild's output"
    );

    let _ = fs::remove_dir_all(&main);
}

/// #826 (the fast path must disagree with the fallback): POISON an UNCHANGED file's grouping row with
/// a wrong `variant_count`. A scoped re-derive editing a DIFFERENT file must leave the poison
/// intact — a whole-repo rebuild would correct it, so its survival proves the scoped pass never
/// re-derived the unchanged path (i.e. work scales with changed files, not repo size).
#[test]
fn the_scoped_logical_rederive_leaves_unchanged_paths_untouched() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn b_one() {}\npub fn b_two() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = db.active_repo_id.clone();
    // Poison b.rs's grouping rows — each b_* function is its own group with variant_count 1.
    db.storage
        .connection()
        .execute(
            "UPDATE main.logical_symbols SET variant_count = 999 WHERE repo_id = ?1 AND path = ?2",
            rusqlite::params![repo_id, "src/b.rs"],
        )
        .unwrap();
    drop(db);

    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\npub fn a_two() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the pass took the scoped re-derive, not a whole-repo rebuild"
    );

    let poisoned: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT variant_count FROM main.logical_symbols WHERE repo_id = ?1 AND path = ?2 \
             LIMIT 1",
            rusqlite::params![repo_id, "src/b.rs"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        poisoned, 999,
        "the UNCHANGED file's poisoned grouping row survives — the scoped pass never touched b.rs"
    );
    assert!(logical_symbol_named(&db, "a_two"), "the edited file WAS regrouped");

    // A whole-repo rebuild DOES correct the poison — proving it was live and fixable, so the scoped
    // pass's decision to skip b.rs is what preserved it.
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    let corrected: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT variant_count FROM main.logical_symbols WHERE repo_id = ?1 AND path = ?2 \
             LIMIT 1",
            rusqlite::params![repo_id, "src/b.rs"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(corrected, 1, "a whole-repo rebuild corrects the poisoned variant_count");

    let _ = fs::remove_dir_all(&main);
}

/// #826 over the OVERLAY finalize (Inline): a key-set change on a LINKED WORKTREE re-derives only
/// that worktree's changed paths' groups, not the whole repo. ZERO whole-repo rebuilds, and the
/// grouping is the wholesale rebuild's fixed point — the scoped re-derive regroups the changed path
/// across ALL its scopes (raw `main.files`), so base + overlay members stay correct.
#[test]
fn a_linked_worktree_overlay_scopes_the_logical_rederive() {
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
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    1\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(logical_symbol_named(&db, "one_fn"), "the added overlay file is grouped");

    // A KEY-SET change on the linked worktree (add two_fn to one.rs, uncommitted → dirty overlay).
    fs::write(
        linked.join("src/one.rs"),
        "pub fn one_fn() -> i32 {\n    1\n}\npub fn two_fn() {}\n",
    )
    .unwrap();
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        0,
        "the overlay finalize's scoped re-derive runs ZERO whole-repo rebuilds"
    );
    assert!(logical_symbol_named(&db, "two_fn"), "the added overlay function is grouped");

    let scoped = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(
        logical_grouping_snapshot(&db),
        scoped,
        "the scoped overlay re-derive output must equal the whole-repo rebuild's output"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #826 gate: a lagging `logical_key_version` owes the #493 drift heal, which ONLY the whole-repo
/// `rebuild_logical_symbols` performs. The scoped re-derive must NOT be used — the pass keeps the
/// full rebuild.
#[test]
fn a_stale_logical_key_version_keeps_the_whole_repo_rebuild() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Force a version lag exactly as the #493 drift-heal tests do.
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();
    drop(db);

    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\npub fn a_two() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "a lagging logical_key_version keeps the whole-repo rebuild (for the #493 drift heal), \
         never the scoped re-derive"
    );
    assert!(logical_symbol_named(&db, "a_two"), "the edit is still grouped");

    let _ = fs::remove_dir_all(&main);
}

/// #826: the scoped re-derive DROPS a group whose symbol is gone from every live scope. Adding a
/// function to a dirty file then removing it leaves that function in NO scope (it was never
/// committed), so the scoped DELETE + regroup must drop its `logical_symbols` row — with ZERO
/// whole-repo rebuilds. (A single dirty deletion of a COMMITTED symbol is deliberately NOT tested
/// here: `logical_symbols` is scope-independent, so the committed symbol's group correctly survives
/// until the deletion is committed — the same for a scoped re-derive and a whole-repo rebuild.)
#[test]
fn the_scoped_rederive_drops_a_removed_symbols_group() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    drop(IndexDatabase::rebuild(&config).unwrap());

    // Add a_two (dirty overlay) — a_two now exists ONLY in the overlay scope.
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\npub fn a_two() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert!(logical_symbol_named(&db, "a_two"), "the added overlay symbol is grouped");
    drop(db);

    // Remove a_two again (the `// edited` line keeps a.rs DIRTY vs the commit, so this stays a
    // scoped dirty re-index rather than a stale-overlay heal). a_two is now gone from EVERY live
    // scope — it was only ever in the overlay.
    fs::write(main.join("src/a.rs"), "// edited\npub fn a_one() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "dropping a symbol takes the scoped re-derive, not a whole-repo rebuild"
    );
    assert!(!logical_symbol_named(&db, "a_two"), "the removed symbol's group is dropped");
    assert!(logical_symbol_named(&db, "a_one"), "the surviving symbol's group is intact");

    // Byte-identity: the drop leaves exactly what a whole-repo rebuild would.
    let scoped = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(logical_grouping_snapshot(&db), scoped, "the drop equals the whole-repo rebuild");

    let _ = fs::remove_dir_all(&main);
}

fn rust_symbol_selector(name: &str) -> rag_rat_query::symbol::SymbolSelector {
    rag_rat_query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some(name.to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    }
}

/// #897: a symbol present in BOTH the base commit and a linked worktree's overlay (same key) has a
/// STORED `variant_count` of 2 (`scope_replica`), but under the linked worktree's scope only ONE
/// member is visible. `symbol_lookup` must report the SCOPE-VISIBLE count (1) / `single` — matching
/// the member list it returns — not the corpus-level stored total.
#[test]
fn overlay_scope_symbol_lookup_reports_scope_visible_variant_count() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn foo() -> i32 {\n    1\n}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Body-only edit keeps foo's key line, so foo is a 2-member scope_replica group (base +
    // overlay).
    fs::write(linked.join("src/a.rs"), "pub fn foo() -> i32 {\n    2\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch body edit"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // index_worktree_overlay leaves the connection in the overlay scope — symbol_lookup sees ONE
    // foo.
    let hit = db.symbol_candidates(&rust_symbol_selector("foo"), false).unwrap();
    assert!(!hit.candidates.is_empty(), "foo resolves in the overlay scope");
    assert_eq!(
        hit.candidates[0].logical_variant_count,
        Some(1),
        "overlay scope sees ONE foo — variant_count must be the scope-visible count, not the \
         corpus total (2)"
    );
    assert_eq!(hit.candidates[0].logical_group_reason.as_deref(), Some("single"));

    // The graph/impact variant LIST must be scoped too, or it would disagree with the count and
    // leak the hidden base member (#897 review): one visible member in the overlay scope, not two.
    let logical_id = hit.candidates[0].logical_symbol_id.expect("logical id");
    let members =
        rag_rat_query::symbol::logical_members(db.storage.connection(), logical_id).unwrap();
    assert_eq!(members.len(), 1, "the scoped variant list matches the scope-visible count (1)");

    // The STORED corpus-level count is still 2: the fix is at surfacing time, not in the table.
    let stored: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT variant_count FROM main.logical_symbols WHERE repo_id = ?1 AND logical_name = \
             ?2",
            rusqlite::params![db.active_repo_id, "foo"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, 2, "the stored corpus-level variant_count remains 2");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #897 (rename half): under the overlay scope, `symbol_lookup` reflects the BRANCH's renamed symbol
/// and NOT the pre-rename name; the base scope is the reverse — per-branch symbol visibility.
#[test]
fn overlay_scope_symbol_lookup_reflects_a_branch_rename() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn target_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn renamed_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "rename"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    let resolves = |db: &IndexDatabase, name: &str| {
        !db.symbol_candidates(&rust_symbol_selector(name), false).unwrap().candidates.is_empty()
    };
    // Overlay scope (left installed by index_worktree_overlay):
    assert!(resolves(&db, "renamed_fn"), "overlay scope sees the branch rename");
    assert!(!resolves(&db, "target_fn"), "overlay scope does not see the pre-rename name");
    // Base scope:
    set_base_scope(&mut db, &main);
    assert!(resolves(&db, "target_fn"), "base scope keeps the original name");
    assert!(!resolves(&db, "renamed_fn"), "base scope does not see the branch rename");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #898: with TWO simultaneously-live worktree overlays holding divergent branch-only content,
/// removing exactly ONE worktree and running gc prunes only that worktree's `files` rows — the
/// other live worktree's overlay rows remain intact.
#[test]
fn gc_prunes_only_the_removed_worktrees_overlay_with_two_live_overlays() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let wt_a = unique_temp_root();
    let _ = fs::remove_dir_all(&wt_a);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-a", wt_a.to_str().unwrap()]);
    fs::write(wt_a.join("src/a_only.rs"), "pub fn a_only() {}\n").unwrap();
    run_git(&wt_a, &["add", "."]);
    run_git(&wt_a, &["commit", "-q", "-m", "branch a"]);
    let wt_b = unique_temp_root();
    let _ = fs::remove_dir_all(&wt_b);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-b", wt_b.to_str().unwrap()]);
    fs::write(wt_b.join("src/b_only.rs"), "pub fn b_only() {}\n").unwrap();
    run_git(&wt_b, &["add", "."]);
    run_git(&wt_b, &["commit", "-q", "-m", "branch b"]);

    db.index_worktree_overlay(&config, &wt_a, &mut |_| {}).unwrap();
    db.index_worktree_overlay(&config, &wt_b, &mut |_| {}).unwrap();
    let a_id = worktree_id_of(&wt_a);
    let b_id = worktree_id_of(&wt_b);
    assert!(
        overlay_rows(&db, &a_id).iter().any(|(path, _)| path == "src/a_only.rs"),
        "worktree A's branch-only file is in its overlay"
    );
    assert!(
        overlay_rows(&db, &b_id).iter().any(|(path, _)| path == "src/b_only.rs"),
        "worktree B's branch-only file is in its overlay"
    );

    // Remove ONLY worktree B, then gc.
    run_git(&main, &["worktree", "remove", "--force", wt_b.to_str().unwrap()]);
    set_base_scope(&mut db, &main);
    db.garbage_collect().unwrap();
    assert!(overlay_rows(&db, &b_id).is_empty(), "gc pruned the removed worktree B's overlay rows");
    assert!(
        overlay_rows(&db, &a_id).iter().any(|(path, _)| path == "src/a_only.rs"),
        "the still-live worktree A's overlay rows are untouched"
    );
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&wt_a);
}

/// #899: a linked worktree that DROPS a symbol via `index_worktree_overlay` (the Inline #826 route,
/// NOT the Deferred batch fallback): the dropped symbol's group is removed by the scoped re-derive,
/// the surviving symbol is kept, ZERO whole-repo rebuilds, and the result is the full rebuild's
/// fixed point. Covers the #826 scoped path for a linked-worktree rename/removal (previously only
/// the addition case exercised the Inline route).
#[test]
fn a_linked_worktree_symbol_drop_scopes_the_logical_rederive() {
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
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // Dirty-add two_fn — it exists ONLY in the overlay (never committed).
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() {}\npub fn two_fn() {}\n").unwrap();
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(logical_symbol_named(&db, "two_fn"), "the dirty-added overlay symbol is grouped");

    // Dirty-remove two_fn (the `// edited` line keeps one.rs dirty vs the feat commit, so this
    // stays the Inline scoped route, not a stale-overlay heal). two_fn is now gone from EVERY
    // live scope.
    fs::write(linked.join("src/one.rs"), "// edited\npub fn one_fn() {}\n").unwrap();
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        0,
        "the drop is served by the Inline scoped re-derive, not a whole-repo rebuild"
    );
    assert!(
        !logical_symbol_named(&db, "two_fn"),
        "the removed overlay symbol's group is dropped via the scoped path"
    );
    assert!(logical_symbol_named(&db, "one_fn"), "the surviving symbol's group is intact");

    let scoped = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(
        logical_grouping_snapshot(&db),
        scoped,
        "the scoped drop equals the whole-repo rebuild"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
