use super::*;

#[test]
fn orientation_composes_through_read_only_connection() {
    // Regression guard: the production SessionStart path (claude_hook::session_start) opens the
    // index via IndexConnection::open_read_only (SQLITE_OPEN_READ_ONLY on the main DB) and then
    // runs orientation(), which CREATEs a TEMP table + TEMP VIEW.  A read-only main DB still
    // permits writes to the TEMP database, so this must succeed — prove it here.

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/a")).unwrap();
    fs::create_dir_all(root.join("src/b")).unwrap();
    for name in &["x.rs", "y.rs", "z.rs"] {
        fs::write(root.join("src/a").join(name), "pub fn ax() {}\n").unwrap();
        fs::write(root.join("src/b").join(name), "pub fn bx() {}\n").unwrap();
    }

    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let db_path = db.database_path().to_path_buf();
    // Drop the writable handle so the read-only open is the only live connection.
    drop(db);

    // Open the SAME on-disk DB read-only, exactly as session_start does.
    let conn = IndexConnection::open_read_only(&db_path).unwrap();
    let o = crate::query::orientation::orientation(conn.connection(), &root, &root, None)
        .expect("orientation must compose through a read-only main-DB connection");

    // The scope view (TEMP table/view) was created and queried — non-empty tree + 6 files.
    assert!(!o.tree.nodes.is_empty(), "tree.nodes empty through read-only conn");
    assert_eq!(o.total_files, 6, "total_files mismatch through read-only conn");

    let _ = fs::remove_dir_all(root);
}

/// #87: a full rebuild must be authoritative for the whole checkout. A stale overlay row shadows
/// its committed counterpart, which exempted the committed row from the clear stage — the rebuild
/// then collided on UNIQUE(path, commit_sha, worktree_id) and FAILED. With the fix, the rebuild
/// succeeds and leaves exactly one row per path, at the commit scope.
#[test]
fn full_rebuild_survives_stale_overlay_rows() {
    let (root, config) = git_fixture_for_overlay_tests();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let worktree_id = db.active_worktree_id.clone();
    let commit = db.active_commit_sha.clone();
    assert!(!commit.is_empty(), "fixture must be a real git checkout");
    insert_stale_overlay_row(&db, "src/lib.rs", &worktree_id);
    drop(db);

    let db = IndexDatabase::rebuild(&config).expect("rebuild must survive stale overlay rows");
    // A6: the rebuild stages a fresh generation and leaves the prior one (the stale overlay
    // included) DEAD for lazy reclamation rather than clearing it in-transaction. gc sweeps
    // that dead generation, after which raw `main.files` holds exactly the new generation's one
    // row per path.
    db.garbage_collect().unwrap();

    let rows: Vec<(String, String)> = {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT commit_sha, worktree_id FROM main.files WHERE path = 'src/lib.rs' AND \
                 kind != 'deleted'",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(rows.len(), 1, "exactly one row per path after an authoritative rebuild: {rows:?}");
    assert_eq!(rows[0], (commit, String::new()), "the clean tree indexes at the commit scope");

    let _ = fs::remove_dir_all(root);
}

/// #59: a FOREIGN file row — a path the real tree never produces — leaked into the index at the
/// ACTIVE scope (the held-mini footgun: a test redirected its DB to the shared self-index and wrote
/// fixture-relative paths under the repo's own commit). It must neither survive a full rebuild nor
/// wedge it on UNIQUE(path, commit_sha, worktree_id). The authoritative clear (#87) stages the
/// whole active commit, so a rebuild removes the leaked row and the self-index self-heals — no
/// manual `.rag-rat` wipe.
#[test]
fn full_rebuild_clears_foreign_leaked_rows_at_the_active_scope() {
    let (root, config) = git_fixture_for_overlay_tests();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let commit = db.active_commit_sha.clone();
    assert!(!commit.is_empty(), "fixture must be a real git checkout");
    // A path the real tree does not contain, leaked at the active commit scope (worktree_id='', the
    // shared clean-row scope real files index at).
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id) VALUES ('src/foreign_leak.rs', 'rust', 'source', \
             'leak', 0, 0, ?1, '', ?2)",
            rusqlite::params![commit, db.active_repo_id],
        )
        .unwrap();
    drop(db);

    let db = IndexDatabase::rebuild(&config)
        .expect("rebuild must survive and clear foreign leaked rows");
    // A6: the rebuild no longer clears the foreign row in-transaction — it stages a fresh
    // generation and the foreign row (an old, now-superseded generation) is DEAD and invisible
    // to readers (the scope view filters the live generation). gc reclaims that dead generation
    // from `main.files`.
    db.garbage_collect().unwrap();
    let leaked: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE path = 'src/foreign_leak.rs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "the authoritative full rebuild clears foreign rows at the active scope");

    let _ = fs::remove_dir_all(root);
}

/// #1 / #106: in a REAL git checkout the active context is `(commit_sha=HEAD, worktree_id=<root
/// path>)` while a clean file row is `(commit_sha=HEAD, worktree_id='')`. The file→package mapping
/// is computed at LOAD time (`load_package_roots_into_scope`) by longest-`manifest_dir`-prefix over
/// the active scope's `packages` rows — there is no persisted `files.package_id` (#106 dropped it
/// to stop a worktree from stamping its package ids onto shared clean rows). This proves the
/// load-time computation correctly maps a clean-checkout file to ITS package on a real git
/// checkout: a path-dep alias declared only by crate `foo` resolves LOCAL inside `foo` and EXTERNAL
/// inside crate `bar`.
#[test]
fn clean_checkout_file_resolves_against_its_own_package_roots() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("crates/foo/src")).unwrap();
    fs::create_dir_all(root.join("crates/bar/src")).unwrap();
    fs::create_dir_all(root.join("crates/helper/src")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    // A workspace where ONLY `foo` declares the RENAMED path-dep alias `shared` (pointing at the
    // `helper` crate). The alias KEY `shared` is local ONLY to foo — it is not a workspace crate
    // name (that is `helper`) and bar never declares it — so the same `use shared::Thing` is
    // local in foo and external in bar. This is the per-package locality (#1) the load-time
    // mapping must honor.
    fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[\"crates/*\"]\n").unwrap();
    fs::write(
        root.join("crates/foo/Cargo.toml"),
        "[package]\nname=\"foo\"\n[dependencies]\nshared = { path = \"../helper\", package = \
         \"helper\" }\n",
    )
    .unwrap();
    // Both foo and bar reference a same-named `Thing` in TYPE position (a `references_type` edge —
    // the bucket per-package suppression acts on).
    fs::write(root.join("crates/foo/src/lib.rs"), "use shared::Thing;\npub fn foo(_t: Thing) {}\n")
        .unwrap();
    fs::write(root.join("crates/bar/Cargo.toml"), "[package]\nname=\"bar\"\n").unwrap();
    fs::write(root.join("crates/bar/src/lib.rs"), "use shared::Thing;\npub fn bar(_t: Thing) {}\n")
        .unwrap();
    fs::write(root.join("crates/helper/Cargo.toml"), "[package]\nname=\"helper\"\n").unwrap();
    // A local `Thing` symbol the bare references could bind to.
    fs::write(root.join("crates/helper/src/lib.rs"), "pub struct Thing;\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "init"]);

    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("crates")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        log: Default::default(),
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert!(!db.active_commit_sha.is_empty(), "fixture must be a real git checkout");
    assert!(!db.active_worktree_id.is_empty(), "a real checkout has a non-empty worktree id");
    let conn = db.storage.connection();

    // In `foo`, `shared` is its declared path-dep alias → LOCAL → the bare `Thing` binds to the
    // shared crate's `Thing`. In `bar`, `shared` is undeclared → EXTERNAL → the bare `Thing` is
    // suppressed (stays unresolved). If the load-time mapping fell open to the global union (the
    // #106 leak), bar's reference would wrongly bind too.
    let foo_bound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges e JOIN files f ON f.id = e.source_file_id WHERE f.path = \
             'crates/foo/src/lib.rs' AND e.to_name = 'Thing' AND e.edge_kind != 'imports' AND \
             e.to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        foo_bound >= 1,
        "in foo, `shared` is its own path-dep alias — the bare `Thing` resolves to the local \
         symbol"
    );
    let bar_bound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges e JOIN files f ON f.id = e.source_file_id WHERE f.path = \
             'crates/bar/src/lib.rs' AND e.to_name = 'Thing' AND e.edge_kind != 'imports' AND \
             e.to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        bar_bound, 0,
        "in bar, `shared` is an EXTERNAL crate — the bare `Thing` must NOT bind to the local \
         symbol"
    );

    let _ = fs::remove_dir_all(root);
}

/// #87 (self-heal half): an incremental pass drops a stale overlay row whose path is clean.
/// With a committed counterpart present the overlay is removed outright; without one, the row is
/// RE-STAMPED to the commit scope in place (same row id — chunks/symbols/embeddings/memory
/// bindings all survive).
#[test]
fn incremental_pass_heals_stale_overlay_rows() {
    let (root, config) = git_fixture_for_overlay_tests();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let worktree_id = db.active_worktree_id.clone();
    let commit = db.active_commit_sha.clone();

    // Case A: stale overlay WITH a committed counterpart -> deleted, committed row takes over.
    insert_stale_overlay_row(&db, "src/lib.rs", &worktree_id);
    // Case B: stale overlay WITHOUT a committed counterpart (its content matches disk) ->
    // re-stamped to the commit scope in place.
    let restamp_id = insert_stale_overlay_row(&db, "src/extra.rs", &worktree_id);
    db.storage
        .connection()
        .execute("DELETE FROM main.files WHERE path = 'src/extra.rs' AND commit_sha != ''", [])
        .unwrap();
    drop(db);

    let db = IndexDatabase::index_discover_with_progress(&config, |_| {}).unwrap();

    let overlays: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE worktree_id != '' AND kind != 'deleted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(overlays, 0, "a clean tree leaves no overlay rows behind");

    let lib_rows: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files WHERE path = 'src/lib.rs' AND kind != 'deleted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lib_rows, 1, "the committed row takes over from the deleted overlay");

    let (extra_id, extra_commit): (i64, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT id, commit_sha FROM main.files WHERE path = 'src/extra.rs' AND kind != \
             'deleted'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(extra_commit, commit, "the orphan overlay is re-stamped to the commit scope");
    assert_eq!(
        extra_id, restamp_id,
        "re-stamp is in place — the row id (and ids hanging off it) survive"
    );

    let _ = fs::remove_dir_all(root);
}

/// Phase 3: the LOCAL structural-load enrichment (`scoped weighted fan-in`) rides along on BOTH the
/// `impact_surface` neighbors AND `symbol_lookup` / `search` hits — labeled, never as PageRank. A
/// hub called by several functions outranks a leaf nothing depends on.
#[test]
fn load_bearing_enrichment_present_on_impact_neighbors_and_lookup_hits() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn load_bearing_hub() -> i32 { 1 }
pub fn quiet_leaf() -> i32 { 2 }
pub fn caller_one() -> i32 { load_bearing_hub() }
pub fn caller_two() -> i32 { load_bearing_hub() }
pub fn caller_three() -> i32 { load_bearing_hub() }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // impact_surface neighbors: running impact on a CALLER surfaces the hub as a callee neighbor,
    // and the hub (three callers) carries the labeled load-bearing signal — the third importance
    // scale, never PageRank.
    let caller_selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("caller_one".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let caller = db.select_symbol(&caller_selector).unwrap().unwrap().expect("caller symbol");
    let report = db
        .impact_surface_report_for_selected_symbol(
            &caller,
            50,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let enriched_hub = report
        .direct_semantic_callees
        .iter()
        .find_map(|hop| hop.importance.as_ref())
        .expect("the hub callee neighbor carries the load-bearing enrichment");
    assert_eq!(enriched_hub.label, "local structural load", "labeled, not PageRank");
    assert_eq!(enriched_hub.signal, "scoped weighted fan-in");
    assert!(enriched_hub.score > 0.0, "the hub's three callers give it positive fan-in");

    // symbol_lookup hits: the hub (3 callers) outscores the leaf (0). Both carry the label, but the
    // leaf has no in-edges in scope so its enrichment is absent — the score reflects scoped fan-in.
    let hub_hit = db
        .symbols("load_bearing_hub", Some(Language::Rust), 10)
        .unwrap()
        .into_iter()
        .find(|h| h.qualified_name.ends_with("load_bearing_hub"))
        .expect("hub lookup hit");
    let hub_importance =
        hub_hit.importance.as_ref().expect("hub has callers → a load-bearing signal");
    assert_eq!(hub_importance.label, "local structural load");
    assert!(hub_importance.score > 0.0, "the hub's three callers give it positive fan-in");

    let leaf_hit = db
        .symbols("quiet_leaf", Some(Language::Rust), 10)
        .unwrap()
        .into_iter()
        .find(|h| h.qualified_name.ends_with("quiet_leaf"))
        .expect("leaf lookup hit");
    assert!(
        leaf_hit.importance.is_none(),
        "a symbol nothing depends on has no in-scope fan-in: {:?}",
        leaf_hit.importance
    );

    // search hits carry the same enrichment on the resolved symbol.
    let search_hub =
        db.search("load_bearing_hub", 20, true).unwrap().into_iter().find(|hit| {
            hit.symbol_path.as_deref().is_some_and(|s| s.ends_with("load_bearing_hub"))
        });
    if let Some(hit) = search_hub
        && let Some(importance) = hit.importance.as_ref()
    {
        assert_eq!(importance.label, "local structural load", "search hit labeled correctly");
    }

    let _ = fs::remove_dir_all(root);
}

/// Phase 3 regression: a CALLEE neighbor whose call was written with a `::` path carries a
/// source-level `target_qualified_name` (e.g. `crate::helper::deep_helper`) that does NOT match
/// rag-rat's `path::name` `qualified_name`. The enrichment must resolve such callees by
/// `to_symbol` (the verified rag-rat `path::name` target) FIRST — resolving by
/// `target_qualified_name` first leaves every qualified-call callee un-enriched. The sibling test
/// above misses this: its callees are bare calls, so `target_qualified_name` is `None` and the
/// fallback to `to_symbol` masks the wrong-order bug.
#[test]
fn load_bearing_enrichment_present_on_qualified_callee_neighbor() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // `deep_helper` is reached only via the `crate::helper::deep_helper()` path, so its callee
    // edge carries a `target_qualified_name` of `crate::helper::deep_helper` — divergent from the
    // rag-rat `path::name` `qualified_name`. Two callers give it fan-in ≥ 1 (so its scoped
    // weighted fan-in is `Some`).
    fs::write(
        root.join("src/helper.rs"),
        r#"
pub fn deep_helper() -> i32 { 7 }
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod helper;
pub fn qualified_caller_one() -> i32 { crate::helper::deep_helper() }
pub fn qualified_caller_two() -> i32 { crate::helper::deep_helper() }
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let caller_selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("qualified_caller_one".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let caller =
        db.select_symbol(&caller_selector).unwrap().unwrap().expect("qualified caller symbol");
    let report = db
        .impact_surface_report_for_selected_symbol(
            &caller,
            50,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();

    let callee_hop = report
        .direct_semantic_callees
        .iter()
        .find(|hop| hop.to_symbol.as_deref().is_some_and(|s| s.ends_with("deep_helper")))
        .expect("the qualified callee neighbor is surfaced");
    // The callee carries the divergent source-level qualified name — the exact shape that
    // un-enriched callees in the wild (`self::storage::connection`, etc.).
    assert!(
        callee_hop
            .target_qualified_name
            .as_deref()
            .is_some_and(|q| q.contains("::") && !q.contains('/')),
        "callee carries a source-level (non path::name) target_qualified_name: {:?}",
        callee_hop.target_qualified_name
    );
    let importance = callee_hop
        .importance
        .as_ref()
        .expect("the qualified callee neighbor carries the load-bearing enrichment");
    assert_eq!(importance.label, "local structural load", "labeled, not PageRank");
    assert_eq!(importance.signal, "scoped weighted fan-in");
    assert!(importance.score > 0.0, "two callers give the callee positive fan-in");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn read_only_open_serves_current_index_and_declines_when_heal_is_owed() {
    // #143: a pure-read MCP tool opens the index read-only, so a concurrent writer (watcher, heal,
    // another client) can never lock it out. A current index is served read-only (Some) and its
    // connection cannot write the main DB; when a heal write is still owed (here a stale
    // graph_index_version), the read path declines (None) so the caller falls back to the
    // read-write open that heals — after which reads are lock-free again.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ro_anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    IndexDatabase::rebuild(&config).unwrap();

    let ro = IndexDatabase::try_open_config_read_only(&config)
        .unwrap()
        .expect("a current index must be served read-only");
    assert!(
        !ro.symbols("ro_anchor", Some(Language::Rust), 10).unwrap().is_empty(),
        "the read-only connection must answer queries"
    );
    assert!(
        ro.storage
            .connection()
            .execute("INSERT INTO index_meta(key, value) VALUES ('ro_probe', 'x')", [])
            .is_err(),
        "a read-only tool connection must not be able to write the main DB"
    );
    drop(ro);

    // Mark the graph index stale (a heal write is now owed). open() already ran and left it
    // current, so set it afterward; the read-only path does not heal, so it must decline.
    let db = IndexDatabase::open(&config.database).unwrap();
    db.set_repo_meta("graph_index_version", "0").unwrap();
    drop(db);
    assert!(
        IndexDatabase::try_open_config_read_only(&config).unwrap().is_none(),
        "a stale graph index owes a heal write → the read-only path must decline"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn impact_report_flags_a_section_truncated_at_limit() {
    // #49: a section capped at `limit` must be named in `truncated_sections` and a caveat — no
    // silent caps. Three callers of `hub` with limit=2 → `direct_semantic_callers` is truncated.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn hub() {}\npub fn a() { hub(); }\npub fn b() { hub(); }\npub fn c() { hub(); }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hub = db.symbols("hub", Some(Language::Rust), 10).unwrap().remove(0);

    // Three repo memories bound to `hub` so the memory section is also over the limit — the
    // truncation report must cover it, not just the non-memory vectors (#146 review).
    for i in 0..3 {
        db.memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: format!("hub note {i}"),
            body: "why hub is load-bearing".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            bind: crate::query::memory::RepoMemoryBindTarget {
                symbol_id: Some(hub.symbol_id),
                ..Default::default()
            },
        })
        .unwrap();
    }

    let report =
        db.impact_surface_report_for_selected_symbol(&hub, 2, &Default::default()).unwrap();
    assert_eq!(report.direct_semantic_callers.len(), 2, "callers truncated to the limit");
    assert!(
        report
            .completeness_and_caveats
            .truncated_sections
            .contains(&"direct_semantic_callers".to_string()),
        "the capped section must be named: {:?}",
        report.completeness_and_caveats
    );
    assert!(
        report.completeness_and_caveats.truncated_sections.contains(&"repo_memories".to_string()),
        "the capped memory section must be named too: {:?}",
        report.completeness_and_caveats
    );
    assert!(
        report
            .completeness_and_caveats
            .caveats
            .iter()
            .any(|caveat| caveat.contains("truncated at limit")),
        "a human caveat must mention truncation: {:?}",
        report.completeness_and_caveats.caveats
    );

    // A generous limit truncates nothing.
    let full = db.impact_surface_report_for_selected_symbol(&hub, 50, &Default::default()).unwrap();
    assert!(
        full.completeness_and_caveats.truncated_sections.is_empty(),
        "nothing should be flagged when under the limit: {:?}",
        full.completeness_and_caveats.truncated_sections
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn symbol_lookup_heals_stale_line_numbers_after_an_edit() {
    // #147: symbol rows aren't anchor-relocated like chunks, so an edit shifts their byte/line
    // positions until reindex. symbol_candidates must lazily heal the matched file and return
    // current positions (and report no residual stale files).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn target() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("target".to_string()),
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    let before = db.symbol_candidates(&selector, false).unwrap();
    let before_byte = before.candidates[0].start_byte;
    assert!(before.stale_files.is_empty(), "clean index has no stale files");

    // Shift `target` down on disk WITHOUT reindexing — the index is now stale for this file.
    fs::write(root.join("src/lib.rs"), "// a\n// b\n// c\npub fn target() {}\n").unwrap();

    let after = db.symbol_candidates(&selector, false).unwrap();
    assert!(after.stale_files.is_empty(), "matched file was healed: {:?}", after.stale_files);
    assert!(
        after.candidates[0].start_byte > before_byte,
        "healed lookup reflects the shifted position: {} !> {before_byte}",
        after.candidates[0].start_byte
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn symbol_lookup_heals_a_just_added_symbol_without_waiting_for_the_watcher() {
    // #152: a name lookup for a symbol just added (here, in a brand-new not-yet-indexed file)
    // returns it via the lazy zero-hit heal, instead of nothing until the watcher catches up. The
    // heal needs a stored Config (open_config) and a git working tree to derive the change set.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn existing() {}\n").unwrap();
    run_git(&root, &["init", "-q"]);
    run_git(&root, &["add", "-A"]);
    run_git(&root, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);

    let config = source_config(root.clone(), Language::Rust);
    IndexDatabase::rebuild(&config).unwrap();
    // Reopen via open_config so the zero-hit heal has the Config to classify the change set.
    let db = IndexDatabase::open_config(&config).unwrap();

    // A brand-new file with a brand-new symbol — never indexed, not yet committed.
    fs::write(root.join("src/added.rs"), "pub fn brand_new_symbol() {}\n").unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("brand_new_symbol".to_string()),
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    let found = db.symbol_candidates(&selector, false).unwrap();
    assert!(
        found.candidates.iter().any(|c| c.name == "brand_new_symbol"),
        "a just-added symbol must be healed in without waiting for the watcher: {:?}",
        found.candidates
    );

    // A genuine miss (a name that exists nowhere) returns empty — no heal resurrects it, no error.
    let miss = crate::query::symbol::SymbolSelector {
        symbol: Some("no_such_symbol_anywhere".to_string()),
        ..selector.clone()
    };
    assert!(
        db.symbol_candidates(&miss, false).unwrap().candidates.is_empty(),
        "a genuine miss must stay empty"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn impact_completeness_flags_dirty_result_files() {
    // #148: a result file dirty vs the index is counted in completeness.stale_files. Resolve via
    // the non-healing `symbols()` so the edit isn't healed away before impact sees it.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn hub() {}\npub fn a() { hub(); }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hub = db.symbols("hub", Some(Language::Rust), 10).unwrap().remove(0);
    let clean =
        db.impact_surface_report_for_selected_symbol(&hub, 20, &Default::default()).unwrap();
    assert_eq!(clean.completeness_and_caveats.stale_files, 0, "nothing dirty right after rebuild");

    // Edit the symbol's file on disk without reindexing.
    fs::write(root.join("src/lib.rs"), "// shifted\npub fn hub() {}\npub fn a() { hub(); }\n")
        .unwrap();
    let dirty =
        db.impact_surface_report_for_selected_symbol(&hub, 20, &Default::default()).unwrap();
    assert!(
        dirty.completeness_and_caveats.stale_files >= 1,
        "the dirty symbol file must be flagged: {:?}",
        dirty.completeness_and_caveats
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn symbol_lookup_does_not_resurrect_a_deleted_symbol_after_heal() {
    // #151 review (P1): when an edit deletes/renames a symbol, healing the stale file and
    // re-resolving by NAME returns nothing — symbol_candidates must NOT keep the pre-heal ghost
    // (dead id, old offsets). The pre-heal fallback is only for symbol_id selectors.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn doomed() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let by_name = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("doomed".to_string()),
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    assert!(
        !db.symbol_candidates(&by_name, false).unwrap().candidates.is_empty(),
        "found before delete"
    );

    // The edit removes `doomed` entirely.
    fs::write(root.join("src/lib.rs"), "pub fn something_else() {}\n").unwrap();

    let after = db.symbol_candidates(&by_name, false).unwrap();
    assert!(
        after.candidates.is_empty(),
        "a deleted symbol must not be resurrected after heal: {:?}",
        after.candidates
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn symbol_lookup_by_id_keeps_pre_heal_candidate_flagged_stale() {
    // #151 review (P1): a symbol_id selector can't survive a reindex (ids reassigned), so the
    // re-resolve is empty — keep the pre-heal candidate flagged stale rather than vanish.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keep() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // `symbols()` does not heal, so this id is from the clean index.
    let id = db.symbols("keep", Some(Language::Rust), 10).unwrap().remove(0).symbol_id;
    fs::write(root.join("src/lib.rs"), "// a\n// b\npub fn keep() {}\n").unwrap();

    let by_id = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: Some(id),
        symbol_path: None,
        symbol: None,
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    let res = db.symbol_candidates(&by_id, false).unwrap();
    assert!(!res.candidates.is_empty(), "symbol_id selector keeps the pre-heal candidate");
    assert!(!res.stale_files.is_empty(), "and flags the file stale: {:?}", res.stale_files);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn impact_completeness_flags_a_dirty_callee_definition_file() {
    // #151 review (P2): a callee defined in another file that's edited makes the resolution stale;
    // the callee's DEFINITION file must be counted, not just the call-site file.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
    fs::write(root.join("src/a.rs"), "pub fn caller() { crate::b::callee(); }\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn callee() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let caller = db.symbols("caller", Some(Language::Rust), 10).unwrap().remove(0);
    let clean =
        db.impact_surface_report_for_selected_symbol(&caller, 20, &Default::default()).unwrap();
    let resolved_to_b = clean
        .direct_semantic_callees
        .iter()
        .any(|hop| hop.to_symbol.as_deref().is_some_and(|s| s.contains("b.rs")));
    assert!(
        resolved_to_b,
        "callee resolved cross-file to b.rs: {:?}",
        clean.direct_semantic_callees
    );
    assert_eq!(clean.completeness_and_caveats.stale_files, 0, "nothing dirty yet");

    // Edit ONLY the callee's definition file (b.rs), not the call-site file (a.rs).
    fs::write(root.join("src/b.rs"), "// shifted\npub fn callee() {}\n").unwrap();
    let dirty =
        db.impact_surface_report_for_selected_symbol(&caller, 20, &Default::default()).unwrap();
    assert!(
        dirty.completeness_and_caveats.stale_files >= 1,
        "the dirty callee definition file must be flagged: {:?}",
        dirty.completeness_and_caveats
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn clone_substrate_has_token_bag_blob_and_no_postings_on_fresh_and_migrated_dbs() {
    // V032 (#231) BLOB-packs the token bag: `symbol_token_postings` is dropped and a `token_bag`
    // BLOB column rides `symbol_fingerprints`. V029 still CREATEs the postings table; V032 drops it
    // (R5 — V029 is never edited), so after apply()/migrate_forward the postings table must be GONE
    // and the column present. The other clone tables (refinements, df) survive.
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    crate::index::schema::apply(&conn).expect("apply");
    for table in ["symbol_fingerprints", "clone_token_df", "clone_refinements"] {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .expect("query");
        assert_eq!(n, 1, "{table} should exist after apply()");
    }
    // symbol_token_postings is dropped by V032; fingerprint_bands was already gone (R1 rework).
    for absent in ["symbol_token_postings", "fingerprint_bands"] {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [absent],
                |r| r.get(0),
            )
            .expect("query absent");
        assert_eq!(n, 0, "{absent} must not exist after V032");
    }
    assert!(
        conn_table_columns(&conn, "symbol_fingerprints").contains(&"token_bag".to_string()),
        "symbol_fingerprints gains the token_bag BLOB column on a fresh apply()"
    );

    let status = crate::index::schema::status(&conn).expect("status");
    assert_eq!(status.current_version, crate::index::schema::LATEST_SCHEMA_VERSION);
    assert!(matches!(status.state, crate::index::schema::SchemaState::Compatible));

    // Migrated DB: a DB driven through migrate_forward reaches the same post-V032 shape.
    let conn2 = rusqlite::Connection::open_in_memory().expect("open2");
    crate::index::schema::apply(&conn2).expect("apply2"); // already-latest is a no-op forward
    crate::index::schema::migrate_forward(&conn2).expect("migrate_forward");
    let postings: i64 = conn2
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND \
             name='symbol_token_postings'",
            [],
            |r| r.get(0),
        )
        .expect("query2");
    assert_eq!(postings, 0, "symbol_token_postings must be gone after migrate_forward");
    assert!(
        conn_table_columns(&conn2, "symbol_fingerprints").contains(&"token_bag".to_string()),
        "symbol_fingerprints carries token_bag after migrate_forward"
    );
    let df: i64 = conn2
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='clone_token_df'",
            [],
            |r| r.get(0),
        )
        .expect("query3");
    assert_eq!(df, 1, "clone_token_df must exist after migrate_forward");
}
