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
    let refresh = crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        Some(&budget),
        &crate::watch::OverlayScope::All,
    );
    assert!(refresh.changed, "the overlay changed (a new branch file was indexed)");

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

/// A `Config` whose single C target binds the WHOLE checkout root (`directories: ["."]`) — the
/// binding `rag-rat init` produces for a repo with no conventional source subdirectory. A
/// `src`-only binding never walks a checkout-root `.cache/`, so it says nothing about the floor.
fn whole_root_c_config(root: PathBuf) -> Config {
    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![ResolvedTarget {
            name: Language::C.as_str().to_string(),
            language: Language::C,
            directories: vec![PathBuf::from(".")],
            include: Language::C.default_include_globs(),
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: rag_rat_base::config::WatchConfig { overlay_quiet_secs: 0, ..Default::default() },
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}

/// Every persisted file row whose path sits under a `.cache/clangd` tree, read RAW from
/// `main.files` — so one call covers the base rows AND every worktree overlay row (and tombstones)
/// at once, rather than only whichever scope view happens to be active.
fn clangd_cache_paths(db: &IndexDatabase) -> Vec<String> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT path FROM main.files
             WHERE path LIKE '.cache/clangd/%' OR instr(path, '/.cache/clangd/') > 0
             ORDER BY path",
        )
        .unwrap();
    let paths = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
    paths.filter_map(Result::ok).collect()
}

#[test]
fn clangd_index_floor_holds_for_both_checkouts_sharing_one_database() {
    // #536: the live clangd oracle runs PER CHECKOUT — the main checkout and each linked worktree
    // spawn their own clangd, and each writes its own `.cache/clangd/` INSIDE that checkout. Both
    // checkouts index into ONE database, so the floor has to hold on the real indexing path in both
    // scopes; two independently-compiled `IgnoreMatcher`s agreeing proves nothing about the rows
    // that actually land.
    //
    // The cache trees hold files with an INDEXABLE extension (`.c`), not clangd's real `.idx`
    // artifacts: `.idx` matches no target extension, so the walker's include filter would drop it
    // on its own and the assertion would hold with the floor deleted.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::create_dir_all(main.join(".cache/clangd/index")).unwrap();
    fs::create_dir_all(main.join(".cache/cmake-build")).unwrap();
    fs::write(main.join("src/shared.c"), "int shared_base(void) { return 0; }\n").unwrap();
    fs::write(main.join("src/main_only.c"), "int main_only_fn(void) { return 1; }\n").unwrap();
    fs::write(
        main.join(".cache/clangd/index/main_side.c"),
        "int main_cache_fn(void) { return 2; }\n",
    )
    .unwrap();
    // Control for the narrowness of the floor AND against a vacuous test: a sibling `.cache/`
    // subtree that is NOT clangd's index must still be indexed, so an empty clangd-cache result
    // cannot be explained by "dot-directories under the root are never walked".
    fs::write(
        main.join(".cache/cmake-build/main_tool.c"),
        "int main_tool_fn(void) { return 3; }\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "-A"]);
    run_git(&main, &["commit", "-q", "-m", "base"]);

    let config = whole_root_c_config(main.clone());
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(names_in_scope(&db, "src/shared.c"), vec!["shared_base".to_string()]);
    assert!(path_in_scope(&db, "src/main_only.c"), "the main checkout indexes its own sources");
    assert!(
        path_in_scope(&db, ".cache/cmake-build/main_tool.c"),
        "the root binding does reach into `.cache/` — only the clangd subtree is floored",
    );
    assert_eq!(
        clangd_cache_paths(&db),
        Vec::<String>::new(),
        "the main checkout's clangd index tree produces no rows",
    );

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Committed (not merely on disk) so the branch↔base tree diff yields each of these as an
    // explicit overlay candidate: the floor, not candidate enumeration, is then the only thing that
    // can keep the linked checkout's own clangd index out of the shared database.
    fs::create_dir_all(linked.join(".cache/clangd/index")).unwrap();
    fs::create_dir_all(linked.join(".cache/cmake-build")).unwrap();
    fs::write(linked.join("src/shared.c"), "int shared_branch(void) { return 0; }\n").unwrap();
    fs::write(linked.join("src/branch_only.c"), "int branch_only_fn(void) { return 4; }\n")
        .unwrap();
    fs::write(
        linked.join(".cache/clangd/index/linked_side.c"),
        "int linked_cache_fn(void) { return 5; }\n",
    )
    .unwrap();
    fs::write(
        linked.join(".cache/cmake-build/branch_tool.c"),
        "int branch_tool_fn(void) { return 6; }\n",
    )
    .unwrap();
    run_git(&linked, &["add", "-A"]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 2, "the branch's changed sources are indexed as overlay rows");

    // The overlay scope: the linked checkout's own sources are present, its clangd index is not,
    // and the main checkout's untouched rows still resolve through the shared base.
    assert_eq!(names_in_scope(&db, "src/shared.c"), vec!["shared_branch".to_string()]);
    assert!(path_in_scope(&db, "src/branch_only.c"), "the branch-only source is in the overlay");
    assert!(
        path_in_scope(&db, ".cache/cmake-build/branch_tool.c"),
        "the overlay pass does reach the branch's `.cache/` — only the clangd subtree is floored",
    );
    assert!(
        path_in_scope(&db, "src/main_only.c"),
        "the sibling checkout's unchanged source is preserved under the overlay scope",
    );

    // The base scope is untouched by the sibling's pass: same symbol, nothing claimed or lost.
    set_base_scope(&mut db, &main);
    assert_eq!(
        names_in_scope(&db, "src/shared.c"),
        vec!["shared_base".to_string()],
        "indexing the linked checkout must not rewrite the main checkout's row",
    );
    assert!(path_in_scope(&db, "src/main_only.c"), "the main checkout keeps its own sources");
    assert!(path_in_scope(&db, ".cache/cmake-build/main_tool.c"));
    assert!(
        !path_in_scope(&db, "src/branch_only.c"),
        "the branch-only source does not leak into the base scope",
    );

    // Raw across BOTH scopes' rows: neither checkout's clangd index reached the database.
    assert_eq!(
        clangd_cache_paths(&db),
        Vec::<String>::new(),
        "neither checkout's `.cache/clangd` tree produces rows in the shared database",
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
fn a_branch_target_survives_an_overlay_root_spelled_with_a_parent_component() {
    // Target containment is enforced at config load, and `for_linked_worktree_overlay` is the one
    // caller that resolves targets against a NON-canonical root: it builds
    // `workdir.join([index] root)` directly, unlike the main load path. A containment check that
    // normalized only the joined target path and compared it against the raw root rejected every
    // genuinely contained directory — and this caller swallows the error and falls back to the base
    // config's targets, so the branch's own targets vanish with no diagnostic at all.
    //
    // The shared-database shape matters here, which is why this is not just a `resolve_targets`
    // unit test: the branch-only target must be indexed into the LINKED checkout's overlay scope,
    // and the base checkout must neither gain those rows nor lose its own.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The branch adds a target directory main does not have, and spells its own `[index] root`
    // with a `..` component — legal, and not canonicalized on this path.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::create_dir_all(linked.join("extra")).unwrap();
    fs::create_dir_all(linked.join("nested")).unwrap();
    fs::write(linked.join("extra/branch_only.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \"nested/..\"\ndatabase = \
         \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = [\"src\", \"extra\"]\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch target"]);

    // The overlay config must carry the BRANCH's targets. Falling back to the base config's here is
    // the silent failure this guards.
    let overlay_config = config.for_linked_worktree_overlay(&linked);
    let overlay_dirs: Vec<_> =
        overlay_config.targets.iter().flat_map(|t| t.directories.iter()).collect();
    assert!(
        overlay_dirs.iter().any(|dir| dir.as_path() == Path::new("extra")),
        "the branch's own target survived a root spelled with `..`: {overlay_dirs:?}",
    );

    db.index_worktree_overlay(&overlay_config, &linked, &mut |_| {}).unwrap();
    assert!(
        names_in_scope(&db, "extra/branch_only.rs").contains(&"branch_only_fn".to_string()),
        "the branch-only target is indexed into the linked checkout's overlay scope",
    );

    // Sibling isolation: the base checkout keeps its own rows and gains none of the branch's.
    set_base_scope(&mut db, &main);
    assert!(
        names_in_scope(&db, "src/base.rs").contains(&"base_fn".to_string()),
        "the base checkout's own rows are preserved",
    );
    assert!(
        !path_in_scope(&db, "extra/branch_only.rs"),
        "a branch-only target must not become visible in the base scope",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn lens_content_hash_follows_the_active_checkout_not_main() {
    // The hash is what an editor compares its own file against before it draws line-anchored
    // signals. Served from the base scope while a linked worktree is active, it would name main's
    // bytes — so a branch checkout whose file genuinely matches its own index would be told it
    // disagrees and go silent, and one that does NOT match main would be told it agrees.
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
    fs::write(linked.join("src/a.rs"), "pub fn branch_fn() {}\n").unwrap();
    fs::write(linked.join("src/only_on_branch.rs"), "pub fn branch_only() {}\n").unwrap();
    run_git(&linked, &["add", "-A"]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    let hash_of = |root: &Path, path: &str| {
        rag_rat_base::hash::hex_sha256(&fs::read(root.join(path)).unwrap())
    };
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.lens_file_content_sha256("src/a.rs").unwrap(),
        Some(hash_of(&linked, "src/a.rs")),
        "the overlay scope names the linked checkout's bytes"
    );
    assert_eq!(
        db.lens_file_content_sha256("src/only_on_branch.rs").unwrap(),
        Some(hash_of(&linked, "src/only_on_branch.rs")),
        "a file only the branch has is still named by its own content"
    );

    set_base_scope(&mut db, &main);
    assert_eq!(
        db.lens_file_content_sha256("src/a.rs").unwrap(),
        Some(hash_of(&main, "src/a.rs")),
        "the base scope names main's bytes, and the two must not be interchangeable"
    );
    assert_eq!(
        db.lens_file_content_sha256("src/only_on_branch.rs").unwrap(),
        None,
        "a branch-only file is absent from the base scope, not silently named by another row"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn an_overlay_refresh_reports_which_checkout_reindexed_which_paths() {
    // The refresh used to answer only "did anything change". That is enough for the pass's own
    // control flow but not for a consumer that must act ON a particular checkout: a resident
    // language server is rooted at one tree and can only answer for that tree (#1010).
    //
    // Two linked worktrees sharing one database, only ONE of them edited: the report must name the
    // edited checkout and its path, and must not attribute that work to its sibling.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let edited = unique_temp_root();
    let quiet = unique_temp_root();
    let _ = fs::remove_dir_all(&edited);
    let _ = fs::remove_dir_all(&quiet);
    run_git(&main, &["worktree", "add", "-q", "-b", "edited", edited.to_str().unwrap()]);
    run_git(&main, &["worktree", "add", "-q", "-b", "quiet", quiet.to_str().unwrap()]);
    // Only the first worktree diverges.
    fs::write(edited.join("src/base.rs"), "pub fn base_fn() { let branch = 1; }\n").unwrap();
    run_git(&edited, &["add", "."]);
    run_git(&edited, &["commit", "-q", "-m", "branch edit"]);

    let refresh = crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    assert!(refresh.changed, "the edited worktree changed, so the pass changed something");
    let edited_id = crate::watch::enclosing_worktree_id(&edited);
    let quiet_id = crate::watch::enclosing_worktree_id(&quiet);
    assert!(
        refresh
            .reindexed
            .get(&edited_id)
            .is_some_and(|entry| entry.paths.iter().any(|path| path.ends_with("src/base.rs"))),
        "the edited checkout must name the path it reindexed: {:?}",
        refresh.reindexed,
    );
    assert!(
        refresh.reindexed.get(&quiet_id).is_none_or(|entry| entry.paths.is_empty()),
        "an unedited sibling must not be credited with the other's work: {:?}",
        refresh.reindexed,
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&edited);
    let _ = fs::remove_dir_all(&quiet);
}

#[test]
fn a_reported_path_resolves_against_the_reported_source_root_not_the_checkout_root() {
    // The paths are config-root-relative (`src/lib.rs`), but the map is keyed by the CHECKOUT
    // root. When `config.root` is a repo subdir the two differ, so a consumer that joined a path
    // onto the key would open `<linked>/src/lib.rs` — which does not exist — instead of
    // `<linked>/crate/src/lib.rs`. The report carries the directory its paths are relative to
    // (#1010).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("crate/src")).unwrap();
    // Canonical root, mirroring `Config::load` — see
    // `worktree_overlay_serves_a_subdir_rooted_config`.
    let main = rag_rat_base::paths::canonicalize(&main).unwrap();
    fs::write(main.join("crate/src/lib.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    // Config rooted at the SUBDIR `crate`.
    let config = source_config(main.join("crate"), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("crate/src/lib.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    let refresh = crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    let linked_id = crate::watch::enclosing_worktree_id(&linked);
    let entry = refresh
        .reindexed
        .get(&linked_id)
        .unwrap_or_else(|| panic!("the linked checkout must be reported: {:?}", refresh.reindexed));
    assert_eq!(entry.paths, vec![PathBuf::from("src/lib.rs")], "paths stay config-root-relative");
    // The property that matters: joining a reported path onto the reported root reaches the file
    // the refresh actually read. Joining onto the checkout root does not.
    let resolved = entry.source_root.join("src/lib.rs");
    assert_eq!(
        fs::read_to_string(&resolved).unwrap(),
        "pub fn linked_fn() {}\n",
        "source_root + path must reach the branch file: {resolved:?}"
    );
    assert!(
        !linked.join("src/lib.rs").exists(),
        "the checkout root is the WRONG base here — that is what this test pins"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn an_overlay_refresh_reports_paths_whose_rows_it_pruned() {
    // Pruning an overlay row un-shadows the base version for that checkout, so the path's
    // effective content changes exactly as much as a write does. Reporting only the files the
    // refresh WROTE would leave a per-checkout consumer serving the branch version of a file that
    // has gone back to the base one, until something unrelated edits it again (#1010).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/base.rs"), "pub fn base_fn() { let branch = 1; }\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch edit"]);
    // First refresh: the divergence produces an overlay row.
    let first = crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    let linked_id = crate::watch::enclosing_worktree_id(&linked);
    assert!(first.reindexed.contains_key(&linked_id), "the divergence must be reported");

    // The branch goes back to the base state: nothing to write, but the stale overlay row is
    // pruned and the base file becomes visible to this checkout again.
    run_git(&linked, &["reset", "-q", "--hard", "HEAD~1"]);
    let second = crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    let entry = second
        .reindexed
        .get(&linked_id)
        .unwrap_or_else(|| panic!("the checkout was visited: {:?}", second.reindexed));
    assert!(
        entry.paths.iter().any(|path| path.ends_with("src/base.rs")),
        "the un-shadowed path must be reported even though the refresh wrote nothing: {entry:?}",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn an_idle_refresh_of_a_diverged_checkout_reports_a_visit_with_no_paths() {
    // A diverged worktree's CANDIDATE set is its whole branch diff, and the refresh re-derives it
    // every sweep — but an unchanged file is identity-skipped and no row moves. Reporting the
    // candidates would name the same files on every pass forever while nothing changed, which is
    // no better for a consumer than re-scanning the checkout and contradicts the empty-entry
    // meaning ("visited, no work"). The list is built from what the transaction committed (#1010).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // A committed divergence, so the branch diff stays NON-EMPTY on every later pass.
    fs::write(linked.join("src/base.rs"), "pub fn base_fn() { let branch = 1; }\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch edit"]);

    let linked_id = crate::watch::enclosing_worktree_id(&linked);
    let first = crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );
    assert!(
        first.reindexed.get(&linked_id).is_some_and(|entry| !entry.paths.is_empty()),
        "the first refresh really did write the diverged file: {:?}",
        first.reindexed,
    );

    // Nothing touched since. The candidate set is unchanged (still the whole branch diff), but
    // every candidate identity-skips.
    let second = crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    assert!(!second.changed, "an idle overlay refresh writes nothing");
    let entry = second
        .reindexed
        .get(&linked_id)
        .unwrap_or_else(|| panic!("the checkout was still VISITED: {:?}", second.reindexed));
    assert!(
        entry.paths.is_empty(),
        "an idle refresh must report the visit with NO paths, not re-report its branch diff: \
         {entry:?}",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_path_scoped_refresh_reports_complete_coverage_despite_an_incomplete_status() {
    // Two different questions share the word "complete". `status_complete` asks "may this
    // refresh's outcome arm the #577 skip?" — always false on the path-scoped route, which never
    // reconciles the whole overlay. `coverage` asks "is the reported path list the whole story?"
    // — true there, because the caller named the exact paths. Deriving the second from the first
    // would mark every event-driven pass lossy and push consumers into a whole-checkout fallback
    // on the hot path (#1010).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // A whole-delta pass first, so the basis is recorded and the next pass can go path-scoped.
    crate::watch::refresh_worktree_overlays(
        &mut db,
        &config,
        None,
        &crate::watch::OverlayScope::All,
    );

    // A dirty edit delivered as an EVENT PATH — the route that takes
    // `index_worktree_overlay_paths`.
    fs::write(linked.join("src/base.rs"), "pub fn base_fn() { let branch = 1; }\n").unwrap();
    let scope = crate::watch::OverlayScope::Paths(BTreeMap::from([(
        linked.clone(),
        BTreeSet::from([linked.join("src/base.rs")]),
    )]));
    let refresh = crate::watch::refresh_worktree_overlays(&mut db, &config, None, &scope);

    let linked_id = crate::watch::enclosing_worktree_id(&linked);
    let entry = refresh.reindexed.get(&linked_id).unwrap_or_else(|| {
        panic!("the event-named checkout must be reported: {:?}", refresh.reindexed)
    });
    assert!(
        entry.paths.iter().any(|path| path.ends_with("src/base.rs")),
        "the event path must be reported: {entry:?}",
    );
    assert_eq!(
        entry.coverage,
        crate::index::ChangedPathsCoverage::Complete,
        "the caller named the exact paths, so nothing is missing from the list: {entry:?}",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
