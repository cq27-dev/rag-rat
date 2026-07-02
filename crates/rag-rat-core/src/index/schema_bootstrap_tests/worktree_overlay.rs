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
    let changed = crate::watch::refresh_worktree_overlays(&mut db, &config, Some(&budget));
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

    let selector = crate::query::symbol::SymbolSelector {
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

    let mut db = IndexDatabase::open(&config.database).unwrap();
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
        db.repo_meta("fts_source_revision").unwrap().as_deref(),
        Some(base_revision.as_str()),
        "fts_source_revision recorded during the overlay pass matches the global digest",
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

    // A FRESH connection (the MCP server querying after the CLI wrote the overlay).
    let mut db = IndexDatabase::open(&config.database).unwrap();
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
        let mut db = IndexDatabase::open(&config.database).unwrap();
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
    let o_wt = crate::query::orientation::orientation(conn.connection(), &main, &linked).unwrap();
    assert_eq!(
        o_wt.total_files, 3,
        "worktree orientation must show base files + the overlay's added file"
    );

    // Main cwd: base scope only = 2.
    let o_main = crate::query::orientation::orientation(conn.connection(), &main, &main).unwrap();
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
