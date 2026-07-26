use super::*;

#[test]
fn anchor_health_counts_tallies_persisted_statuses() {
    // Exercise the GROUP BY query in anchor_health_counts and the active-only filter.
    // Create two memories bound to real symbols; after memory_validate they should both be
    // "current". Assert memory_anchor_health() returns current >= 2 and gone == 0.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn health_alpha() {}\npub fn health_beta() {}\n")
        .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let sym = |name: &str| {
        db.select_symbol(&rag_rat_query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some(name.to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("symbol must exist")
    };
    let alpha = sym("health_alpha");
    let beta = sym("health_beta");

    let bind_target = |symbol_id| rag_rat_query::memory::RepoMemoryBindTarget {
        symbol_id: Some(symbol_id),
        logical_symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir: None,
    };

    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "health alpha invariant".to_string(),
        body: "Anchor health test — alpha binding.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: bind_target(alpha.symbol_id),
    })
    .unwrap();

    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "health beta decision".to_string(),
        body: "Anchor health test — beta binding.".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: bind_target(beta.symbol_id),
    })
    .unwrap();

    // Validate so bindings get their anchor_status written to "current".
    db.memory_validate().unwrap();

    let health = db.memory_anchor_health().unwrap();
    assert!(health.current >= 2, "expected at least 2 current bindings, got {health:?}");
    assert_eq!(health.gone, 0, "expected no gone bindings, got {health:?}");
    assert_eq!(health.stale, 0, "expected no stale bindings, got {health:?}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn memory_doctor_lists_gone_and_suggests_candidates() {
    // Bind a memory to `fn doctor_src` in a.rs. Delete a.rs and add `fn doctor_src` to b.rs
    // with a different body (so content-hash relocation does NOT fire and the binding stays
    // gone). Then call `memory_doctor`: the entry must appear with anchor_status == "gone"
    // and a non-empty candidate list (the same-named fn in b.rs).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn doctor_src() -> u32 {\n    1\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let src_symbol = db
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("doctor_src".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("doctor_src in a.rs");

    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "doctor test memory".to_string(),
        body: "This memory is bound to a symbol that will become gone.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            symbol_id: Some(src_symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            tracker: None,
            project: None,
            item_key: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // Remove a.rs and add b.rs with the same-named fn but a different body (hash mismatch
    // intentional — content relocation must NOT fire, leaving the binding gone).
    fs::remove_file(root.join("src/a.rs")).unwrap();
    fs::write(root.join("src/b.rs"), "pub fn doctor_src() -> u32 {\n    99\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Twice: the #492 downgrade hysteresis defers the first gone observation, and doctor reads
    // the PERSISTED status.
    db.memory_validate().unwrap();
    let validate_report = db.memory_validate().unwrap();
    assert_eq!(
        validate_report.gone, 1,
        "binding must be gone after removing a.rs: {validate_report:?}"
    );

    // Now run memory_doctor and verify the entry is present with a candidate.
    let entries = db.memory_doctor().unwrap();
    assert_eq!(entries.len(), 1, "doctor should return exactly one entry: {entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.title, "doctor test memory");
    assert!(
        entry.anchor_status == "gone" || entry.anchor_status == "stale",
        "anchor_status should be gone or stale, got: {}",
        entry.anchor_status
    );
    // The same-named fn in b.rs must appear as a candidate.
    assert!(
        !entry.candidates.is_empty(),
        "doctor entry must have at least one candidate for the same-named fn in b.rs: {entry:?}"
    );
    assert!(
        entry.candidates.iter().any(|c| c.contains("doctor_src")),
        "candidate must contain 'doctor_src': {:?}",
        entry.candidates
    );

    let _ = fs::remove_dir_all(&root);
}

/// A memory stranded under the `'__unassigned__'` placeholder on an ADOPTED DB — the V042
/// consolidated-DB backfill's leave-at-placeholder path — is user-authored data invisible to
/// every scoped memory read. The doctor must surface it as a `placeholder_repo` entry instead of
/// letting it vanish silently. (Needs a REAL git fixture: on a placeholder-active DB the
/// placeholder scope is the normal state and the doctor deliberately stays quiet about it.)
#[test]
fn memory_doctor_surfaces_placeholder_scoped_memories() {
    let (_root, config) = super::poison_test_config("doctor_placeholder");
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Strand a memory under the placeholder, exactly as the V042 backfill leaves one on a
    // consolidated DB.
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
                 memory_version, repo_id)
             VALUES ('mem_placeholder', 'Invariant', 'stranded memory', 'body', 'high', 'active', \
             0, 0, 'manual', 'v1', ?1)",
            [rag_rat_base::repo_identity::LEGACY_REPO_ID],
        )
        .unwrap();

    let entries = db.memory_doctor().unwrap();
    let entry = entries
        .iter()
        .find(|e| e.memory_id == "mem_placeholder")
        .expect("the placeholder-scoped memory must be surfaced by the doctor");
    assert_eq!(entry.anchor_status, "placeholder_repo");
    assert_eq!(entry.title, "stranded memory");
    assert_eq!(entry.binding_kind, "repo");
    assert!(entry.candidates.is_empty(), "no computable rebind candidates for a repo strand");
}

#[test]
fn memory_doctor_dedupes_cfg_split_candidates() {
    // A gone binding whose same-name symbol is cfg-split must surface that candidate ONCE — the
    // bare-name candidate query returns a row per physical twin, and the rebind suggestion is by
    // qualified name, so undeduped twins would print the identical command twice.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Bind to a plain (non-cfg) helper in a.rs.
    fs::write(root.join("src/a.rs"), "pub fn cfg_helper() -> u32 {\n    1\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let original = db
        .symbol_candidates(
            &rag_rat_query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("cfg_helper".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap()
        .candidates[0]
        .symbol_id;
    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "cfg helper note".to_string(),
        body: "Bound to a helper that becomes a cfg-split pair in another file.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            symbol_id: Some(original),
            ..Default::default()
        },
    })
    .unwrap();

    // Remove a.rs and reintroduce `cfg_helper` as a cfg-split pair in b.rs with DIFFERENT bodies,
    // so content-hash relocation cannot fire (binding goes gone) while the qualified name survives
    // as two physical twins sharing one logical symbol.
    fs::remove_file(root.join("src/a.rs")).unwrap();
    fs::write(
        root.join("src/b.rs"),
        "#[cfg(not(target_arch = \"wasm32\"))]\npub fn cfg_helper() -> u32 {\n    \
         11\n}\n\n#[cfg(target_arch = \"wasm32\")]\npub fn cfg_helper() -> u32 {\n    22\n}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Twice: the #492 downgrade hysteresis defers the first gone observation, and doctor reads
    // the PERSISTED status.
    db.memory_validate().unwrap();
    assert_eq!(db.memory_validate().unwrap().gone, 1, "binding must be gone");

    let entries = db.memory_doctor().unwrap();
    let entry = entries.iter().find(|e| e.title == "cfg helper note").expect("doctor entry");
    let cfg_candidates: Vec<&String> =
        entry.candidates.iter().filter(|c| c.ends_with("cfg_helper")).collect();
    assert_eq!(cfg_candidates.len(), 1, "cfg twins collapse to one suggestion: {cfg_candidates:?}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbol_path_selector_is_exact_not_substring() {
    // `--symbol-path` (the qualified-name route the doctor now suggests) must match exactly:
    // the qualified name `…::spawn_blocking` must NOT also pull in `spawn_blocking_handle` /
    // `spawn_blocking_offload`. This is what makes the doctor's suggestion runnable.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn spawn_blocking() {}\npub fn spawn_blocking_handle() {}\npub fn \
         spawn_blocking_offload() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hit = db
        .select_symbol_for_bind(&rag_rat_query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: Some("src/lib.rs::spawn_blocking".to_string()),
            symbol: None,
            language: None,
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .expect("exact qualified name resolves, no substring siblings")
        .expect("one hit");
    assert_eq!(hit.name, "spawn_blocking");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn select_symbol_for_bind_collapses_cfg_split_group() {
    // The memory-doctor bug: a memory bound to a cfg-split helper goes gone, and the suggested
    // `--symbol <qualified_name>` rebind hits BOTH cfg twins → ambiguous → dead end. The
    // bind-resolution path must collapse a one-logical-group candidate set to a single member so
    // the rebind succeeds, while a genuinely-distinct same-name set still disambiguates.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(not(target_arch = \"wasm32\"))]\npub fn spawn_blocking() {}\n\n#[cfg(target_arch = \
         \"wasm32\")]\npub fn spawn_blocking() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Resolve by the fully-qualified name the doctor would suggest. select_symbol (no collapse)
    // must disambiguate; select_symbol_for_bind must collapse to one member of the logical group.
    let qualified = db
        .symbol_candidates(
            &rag_rat_query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("spawn_blocking".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap()
        .candidates[0]
        .qualified_name
        .clone();
    let logical_id = db
        .symbol_candidates(
            &rag_rat_query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("spawn_blocking".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap()
        .candidates[0]
        .logical_symbol_id
        .expect("cfg twins share a logical id");

    let selector = rag_rat_query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: Some(qualified.clone()),
        symbol: None,
        language: None,
        allow_ambiguous: false,
        limit: 10,
    };
    assert!(
        db.select_symbol(&selector).unwrap().is_err(),
        "plain select_symbol must still disambiguate the two cfg twins"
    );
    let hit = db
        .select_symbol_for_bind(&selector)
        .unwrap()
        .expect("cfg group collapses, not ambiguous")
        .expect("one member returned");
    assert_eq!(hit.logical_symbol_id, Some(logical_id));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn repo_brief_ranks_churn_and_god_module_candidates() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);

    fs::write(root.join("src/stable.rs"), "pub fn stable() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("src/hot.rs"), hot_module_text(0)).unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add initial modules"]);

    for revision in 1..=3 {
        fs::write(root.join("src/hot.rs"), hot_module_text(revision)).unwrap();
        run_git(&root, &["add", "src/hot.rs"]);
        run_git(&root, &["commit", "-m", "Iterate hot module"]);
    }

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
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
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

    let churn = db
        .repo_brief(rag_rat_query::repo_brief::RepoBriefOptions {
            mode: rag_rat_query::repo_brief::RepoBriefMode::Churn,
            limit: 1,
            include_generated: false,
            include_memories: true,
        })
        .unwrap();
    assert_eq!(churn.candidates[0].path, "src/hot.rs");
    assert_eq!(churn.candidates[0].category, "recent_churn_hotspot");
    assert!(churn.candidates[0].score <= 1.0);
    assert!(churn.candidates[0].metrics.commit_touch_count >= 4);
    assert!(churn.candidates[0].why.iter().any(|reason| reason.contains("churn")));

    let god_modules = db
        .repo_brief(rag_rat_query::repo_brief::RepoBriefOptions {
            mode: rag_rat_query::repo_brief::RepoBriefMode::GodModules,
            limit: 1,
            include_generated: false,
            include_memories: true,
        })
        .unwrap();
    assert_eq!(god_modules.candidates[0].path, "src/hot.rs");
    assert!(god_modules.candidates[0].score <= 1.0);
    assert!(god_modules.candidates[0].metrics.symbol_count >= 30);
    assert!(!god_modules.candidates[0].split_hints.is_empty());
    assert!(god_modules.candidates[0].next_tools.iter().any(|tool| tool.tool == "impact_surface"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn repo_clusters_groups_cotouched_files() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/sync")).unwrap();
    fs::create_dir_all(root.join("src/ui")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);

    fs::write(root.join("src/sync/actor.rs"), "pub fn sync_actor() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("src/sync/msg.rs"), "pub fn sync_msg() -> i32 { 2 }\n").unwrap();
    fs::write(root.join("src/ui/app.rs"), "pub fn ui_app() -> i32 { 3 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add modules"]);

    for revision in 1..=2 {
        fs::write(
            root.join("src/sync/actor.rs"),
            format!("pub fn sync_actor() -> i32 {{ {revision} }}\n"),
        )
        .unwrap();
        fs::write(
            root.join("src/sync/msg.rs"),
            format!("pub fn sync_msg() -> i32 {{ {} }}\n", revision + 10),
        )
        .unwrap();
        run_git(&root, &["add", "src/sync/actor.rs", "src/sync/msg.rs"]);
        run_git(&root, &["commit", "-m", "Iterate sync modules"]);
    }

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
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
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

    let clusters = db
        .repo_clusters(crate::query::clusters::RepoClustersOptions {
            limit: 5,
            include_generated: false,
            include_memories: true,
            min_cluster_size: 2,
        })
        .unwrap();

    let sync_cluster =
        clusters.clusters.iter().find(|cluster| cluster.name == "src/sync").expect("sync cluster");
    assert!(sync_cluster.representative_paths.contains(&"src/sync/actor.rs".to_string()));
    assert!(sync_cluster.representative_paths.contains(&"src/sync/msg.rs".to_string()));
    assert!(sync_cluster.metrics.co_touch_edges >= 2);

    let _ = fs::remove_dir_all(&root);
}
