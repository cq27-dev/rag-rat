use super::*;

#[test]
fn repo_memory_bound_to_logical_symbol_surfaces_in_symbol_chunk_and_impact() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn cfg_helper() {}\n#[cfg(windows)]\npub fn cfg_helper() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("cfg_helper".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");
    let logical_symbol_id = symbol.logical_symbol_id.expect("logical symbol id");

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Treat cfg helper variants as one logical helper".to_string(),
            body: "Caller and impact analysis should use the logical symbol, not one cfg body \
                   variant."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["cfg".to_string(), "graph".to_string()],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(logical_symbol_id),
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert!(!created.duplicate);
    assert_eq!(created.memory.bindings[0].binding_kind, "logical_symbol");

    let memories = db.memory_for_symbol(&symbol, 10).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].kind, "Invariant");
    let chunk_id = memories[0].bindings[0].chunk_id.expect("bound chunk");
    let chunk = db.read_chunk(chunk_id).unwrap().expect("memory chunk");
    assert_eq!(chunk.memories.len(), 1);
    assert_eq!(chunk.memories[0].memory_id, created.memory.memory_id);

    let impact = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    assert_eq!(impact.repo_memories.compact().unwrap().direct.len(), 1);
    assert_eq!(impact.completeness_and_caveats.memory_status.active, 1);
    assert_eq!(impact.completeness_and_caveats.memory_status.stale, 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn compact_repo_memory_view_projects_primary_binding_then_full_mode_round_trips() {
    // #37: the default `impact_surface` memory output is the scannable compact projection of each
    // memory's primary binding; full bodies + bindings stay one explicit flag away.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchored() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("anchored".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");
    let logical_symbol_id = symbol.logical_symbol_id.expect("logical symbol id");
    let full_body = "Runtime shutdown must be idempotent; second call is a no-op.".to_string();
    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Runtime shutdown must be idempotent".to_string(),
            body: full_body.clone(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["runtime".to_string()],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(logical_symbol_id),
                ..Default::default()
            },
        })
        .unwrap();

    // Default mode is compact: a scannable header projected from the primary binding.
    let compact_report = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let compact = compact_report.repo_memories.compact().expect("compact by default");
    assert!(compact_report.repo_memories.full().is_none(), "default must not be full");
    assert_eq!(compact.direct.len(), 1);
    let entry = &compact.direct[0];
    assert_eq!(entry.memory_id, created.memory.memory_id);
    assert_eq!(entry.kind, "Invariant");
    assert_eq!(entry.title, "Runtime shutdown must be idempotent");
    assert_eq!(entry.confidence, "high");
    assert_eq!(entry.status, "active");
    assert_eq!(entry.anchor_status.as_deref(), Some("current"));
    assert_eq!(entry.binding_kind.as_deref(), Some("logical_symbol"));
    assert_eq!(entry.path.as_deref(), Some("src/lib.rs"));
    assert!(entry.span.is_some(), "logical-symbol binding carries a line span");
    assert_eq!(entry.logical_symbol_id, Some(logical_symbol_id));
    assert_eq!(entry.tags, vec!["runtime".to_string()]);

    // Explicit full mode restores the body + full bindings for deep inspection.
    let full_report = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions {
                compact_memories: false,
                ..Default::default()
            },
        )
        .unwrap();
    let full = full_report.repo_memories.full().expect("full on request");
    assert!(full_report.repo_memories.compact().is_none(), "full mode is not compact");
    assert_eq!(full.direct.len(), 1);
    assert_eq!(full.direct[0].body, full_body);
    assert_eq!(full.direct[0].bindings[0].binding_kind, "logical_symbol");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn compact_repo_memory_view_separates_the_stale_lane() {
    // #37: a memory whose anchor went stale lands in the compact `stale` lane (not `direct`), with
    // its `anchor_status` carried through so an agent can see it needs re-anchoring.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn drifting() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("drifting".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");
    let chunk_id = db
        .storage
        .connection()
        .query_row(
            "
                SELECT chunks.id
                FROM chunks
                JOIN files ON files.id = chunks.file_id
                WHERE files.path = ?1 AND chunks.symbol_path = ?2
                LIMIT 1
                ",
            params![symbol.path, symbol.qualified_name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchor drifts when the chunk hash changes".to_string(),
            body: "Stale anchors belong in their own lane, away from current evidence.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                chunk_id: Some(chunk_id),
                ..Default::default()
            },
        })
        .unwrap();

    // Current anchor: the memory is in the active `direct` lane, stale lane empty.
    let before = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let before = before.repo_memories.compact().unwrap();
    assert_eq!(before.direct.len(), 1);
    assert!(before.stale.is_empty());

    // Drift the underlying chunk so validation marks the binding stale.
    db.storage
        .connection()
        .execute("UPDATE chunks SET text_hash = 'changed' WHERE id = ?1", [chunk_id])
        .unwrap();
    assert_eq!(db.memory_validate().unwrap().stale, 1);

    let after = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            10,
            &crate::query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let after = after.repo_memories.compact().unwrap();
    assert!(after.direct.is_empty(), "a stale memory leaves the direct lane");
    assert_eq!(after.stale.len(), 1);
    assert_eq!(after.stale[0].memory_id, created.memory.memory_id);
    assert_eq!(after.stale[0].anchor_status.as_deref(), Some("stale"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn repo_memory_survives_reindex_and_relocates_when_symbol_moves() {
    // The user-facing guarantee: a memory is never lost to reindexing (no FK cascade from
    // symbols/chunks), and a symbol binding re-anchors to the symbol's new location when the
    // file is edited/moved rather than going stale.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keystone() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let selector = crate::query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("keystone".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol");
    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "keystone holds an invariant".to_string(),
            body: "This memory must survive a reindex and follow the symbol when it moves."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();

    // Edit the file so keystone moves down (new symbol ids on reindex), then rebuild.
    fs::write(root.join("src/lib.rs"), "pub fn added_above() {}\n\npub fn keystone() {}\n")
        .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Memory row survives the reindex (no cascade from deleted symbols).
    assert!(
        crate::query::memory::memory_by_id(db.storage.connection(), &created.memory.memory_id,)
            .unwrap()
            .is_some(),
        "memory was lost to reindex",
    );

    // Re-validation re-anchors the binding to keystone's new location, not "gone".
    db.memory_validate().unwrap();
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol after move");
    let anchored = db.memory_for_symbol(&symbol, 10).unwrap();
    assert_eq!(anchored.len(), 1, "memory did not re-anchor to moved symbol");
    assert_ne!(anchored[0].bindings[0].anchor_status, "gone");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn repo_memory_validate_marks_changed_or_missing_anchors_non_current() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchored_memory() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("anchored_memory".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");
    let chunk_id = db
        .storage
        .connection()
        .query_row(
            "
                SELECT chunks.id
                FROM chunks
                JOIN files ON files.id = chunks.file_id
                WHERE files.path = ?1 AND chunks.symbol_path = ?2
                LIMIT 1
                ",
            params![symbol.path, symbol.qualified_name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchor must become stale when source hash changes".to_string(),
            body: "Validation should separate stale memories from current repo evidence."
                .to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: Some(chunk_id),
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();

    db.storage
        .connection()
        .execute("UPDATE chunks SET text_hash = 'changed' WHERE id = ?1", [chunk_id])
        .unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.stale, 1);
    let stale = db.memory_for_symbol(&symbol, 10).unwrap();
    assert_eq!(stale[0].memory_id, created.memory.memory_id);
    assert_eq!(stale[0].bindings[0].anchor_status, "stale");

    db.storage.connection().execute("DELETE FROM chunks WHERE id = ?1", [chunk_id]).unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1);
    let gone = db.memory_for_symbol(&symbol, 10).unwrap();
    assert_eq!(gone[0].bindings[0].anchor_status, "gone");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn repo_memory_bound_to_edge_surfaces_when_impact_crosses_call_path() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn target_edge() {}\npub fn caller_edge() {\n    target_edge();\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let target = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target_edge".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected target");
    let graph_options = crate::query::graph::GraphTraversalOptions {
        resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
        symbol_id: Some(target.symbol_id),
        logical_symbol_id: target.logical_symbol_id,
        ..Default::default()
    };
    let callers =
        db.graph_traversal_report("find_callers", &target, true, 10, &graph_options).unwrap();
    let edge_id = callers.results[0].edge_id;

    let edge_memory = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "caller_edge to target_edge must stay synchronous".to_string(),
            body: "This specific call path is used to prove edge-bound memories surface when \
                   impact crosses the edge."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["edge".to_string()],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: Some(edge_id),
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert_eq!(edge_memory.memory.bindings[0].binding_kind, "edge");
    assert_eq!(edge_memory.memory.bindings[0].edge_id, Some(edge_id));

    let impact = db
        .impact_surface_report_for_selected_symbol(
            &target,
            10,
            &crate::query::impact::ImpactSurfaceOptions {
                resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                ..Default::default()
            },
        )
        .unwrap();
    let compact = impact.repo_memories.compact().unwrap();
    assert!(compact.direct.is_empty());
    assert_eq!(compact.path_crossed.len(), 1);
    assert_eq!(compact.path_crossed[0].memory_id, edge_memory.memory.memory_id);
    assert_eq!(impact.completeness_and_caveats.memory_status.active, 1);

    let call_path_memory = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "TestExpectation".to_string(),
            title: "caller_edge path hash recall".to_string(),
            body: "Call-path memories are addressable by a deterministic edge sequence hash."
                .to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["call-path".to_string()],
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: target.logical_symbol_id,
                end_logical_symbol_id: target.logical_symbol_id,
                edge_sequence_hash: Some("edge-sequence-test-hash".to_string()),
                path_summary: Some("caller_edge -> target_edge".to_string()),
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    let call_path = db.memory_for_call_path_hash("edge-sequence-test-hash", 10).unwrap();
    assert_eq!(call_path.len(), 1);
    assert_eq!(call_path[0].memory_id, call_path_memory.memory.memory_id);
    assert_eq!(call_path[0].call_paths[0].path_summary, "caller_edge -> target_edge");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn server_derived_call_path_hash_is_stable_and_validates_through_edge_churn() {
    // #38: bind a call-path memory by ordered edge ids — the server derives the authoritative
    // edge_sequence_hash from edge fingerprints. A full rebuild reassigns edge row ids, but the
    // hash (built from row-id-independent fingerprints) is unchanged and validation stays
    // "current". Deleting the call site makes the path "gone".
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn caller() {\n    callee();\n}\npub fn callee() {}\n")
        .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge_id = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT id FROM edges WHERE to_name LIKE '%callee%' ORDER BY id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("caller->callee edge present")
    };
    let call_path_status = |db: &IndexDatabase| -> String {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status FROM repo_memory_bindings WHERE binding_kind = 'call_path' \
                 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap()
    };

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "why callee is invoked here".to_string(),
            body: "This call path is load-bearing.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: Some(vec![edge_id(&db)]),
                dir: None,
            },
        })
        .unwrap();

    // The stored binding_id is the server-derived hash, and it created at "current".
    let hash: String = db
        .storage
        .connection()
        .query_row(
            "SELECT binding_id FROM repo_memory_bindings WHERE binding_kind = 'call_path' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(call_path_status(&db), "current");
    // memory_for_call_path resolves the server hash.
    let found = db.memory_for_call_path_hash(&hash, 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].memory_id, created.memory.memory_id);

    // Rebuild reassigns edge row ids; the fingerprint-derived hash and "current" status survive.
    let old_edge = edge_id(&db);
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_ne!(
        edge_id(&db),
        old_edge,
        "rebuild must reassign the edge row id for a real churn test"
    );
    db.memory_validate().unwrap();
    assert_eq!(call_path_status(&db), "current", "server hash survives edge row-id churn");
    assert_eq!(db.memory_for_call_path_hash(&hash, 10).unwrap().len(), 1);

    // Move the call site down a line: the source line (and thus the exact fingerprint) changes,
    // but the edge's loose identity (caller -> callee) still matches → relocated, not gone.
    fs::write(
        root.join("src/lib.rs"),
        "// shift\n\npub fn caller() {\n    callee();\n}\npub fn callee() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.memory_validate().unwrap();
    assert_eq!(call_path_status(&db), "relocated", "a moved call site relocates the path");

    // Remove the call site → the edge is gone → the call path is gone.
    fs::write(root.join("src/lib.rs"), "pub fn caller() {}\npub fn callee() {}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.memory_validate().unwrap();
    assert_eq!(call_path_status(&db), "gone", "deleting the call site makes the path gone");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn impact_surface_surfaces_call_path_memory_when_path_crossed() {
    // #38 (acceptance #1 + #3): a call-path memory bound to the server-derived hash of
    // a -> b -> c surfaces in impact_surface(b).repo_memories.call_path_crossed, because the
    // traversal crosses the caller edge (a -> b) and the callee edge (b -> c).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn a() {\n    b();\n}\npub fn b() {\n    c();\n}\npub fn c() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge_to = |name: &str| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT id FROM edges WHERE to_name = ?1 ORDER BY id LIMIT 1",
                [name],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("edge to `{name}` present: {e}"))
    };
    let caller_edge = edge_to("b"); // a -> b
    let callee_edge = edge_to("c"); // b -> c

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "a -> b -> c is the hot path".to_string(),
        body: "Why this two-hop path matters.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: Some(vec![caller_edge, callee_edge]),
            dir: None,
        },
    })
    .unwrap();

    let symbol_b = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: Some("src/lib.rs::b".to_string()),
            symbol: None,
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("symbol b");

    let report = crate::query::impact::impact_surface_report_for_symbol(
        db.storage.connection(),
        &symbol_b,
        10,
        &crate::query::impact::ImpactSurfaceOptions::default(),
        |_hops| Ok(false),
    )
    .unwrap();

    let compact = report.repo_memories.compact().unwrap();
    assert!(
        compact
            .call_path_crossed
            .iter()
            .any(|memory| memory.title == "a -> b -> c is the hot path"),
        "call-path memory should surface in impact_surface(b); got call_path_crossed = {:?}",
        compact.call_path_crossed.iter().map(|m| &m.title).collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn memory_relocates_when_symbol_moves_to_another_file() {
    // Bind a memory to `fn target` in a.rs; move `fn target` verbatim to b.rs; reindex.
    // The cross-file bare-name + content-hash fallback must fire: relocated == 1, gone == 0,
    // and the persisted binding path is now b.rs.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Write the symbol in a.rs; keep b.rs present but empty so the indexer knows about it.
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("target symbol in a.rs");
    assert!(symbol.path.contains("a.rs"), "initial path should be a.rs: {}", symbol.path);

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "target returns 42".to_string(),
            body: "This memory must follow target across a file move.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert_eq!(created.memory.bindings[0].binding_kind, "symbol");

    // Move `fn target` verbatim to b.rs; remove it from a.rs.
    fs::write(root.join("src/a.rs"), "// target moved to b.rs\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let report = db.memory_validate().unwrap();
    assert_eq!(report.relocated, 1, "expected 1 relocated binding, report: {report:?}");
    assert_eq!(report.gone, 0, "expected 0 gone bindings, report: {report:?}");

    // The binding must now point at b.rs.
    let binding = &db
        .memory_for_symbol(
            &db.select_symbol(&crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("target".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: false,
                limit: 10,
            })
            .unwrap()
            .unwrap()
            .expect("target in b.rs"),
            10,
        )
        .unwrap()[0]
        .bindings[0]
        .clone();
    let path = binding.path.as_deref().unwrap_or("");
    assert!(path.contains("b.rs"), "binding path should be b.rs after relocation: {path}");
    assert_ne!(binding.anchor_status, "gone");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn memory_relocation_is_durable_across_a_second_reindex() {
    // After a cross-file move+relocate, a subsequent reindex (with an unrelated edit to b.rs)
    // must resolve via the rewritten qualified_name directly — not fall back to the bare-name
    // relocation path again. The binding_id must equal b.rs::target (not the old a.rs::target).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("target in a.rs");

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "target durable across reindex".to_string(),
        body: "After relocation the binding must stay stable on a second reindex.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // First reindex: move target verbatim from a.rs to b.rs.
    fs::write(root.join("src/a.rs"), "// moved\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report1 = db.memory_validate().unwrap();
    assert_eq!(report1.relocated, 1, "first validate should relocate: {report1:?}");

    // Second reindex: add an unrelated symbol to b.rs, leaving target's body unchanged.
    fs::write(
        root.join("src/b.rs"),
        "pub fn target() -> u32 {\n    42\n}\npub fn unrelated() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report2 = db.memory_validate().unwrap();
    // Must NOT be gone — resolves via the rewritten binding_id (b.rs::target).
    assert_eq!(report2.gone, 0, "binding should not be gone after second reindex: {report2:?}");

    // Confirm binding_id now points at b.rs (the relocation was persisted).
    let binding = db
        .storage
        .connection()
        .query_row(
            "SELECT binding_id FROM repo_memory_bindings WHERE binding_kind = 'symbol' LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert!(
        binding.contains("b.rs"),
        "binding_id should be the new b.rs qualified_name after relocation, got: {binding}"
    );
    assert!(!binding.contains("a.rs"), "binding_id must not still reference a.rs: {binding}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn relocation_persists_refreshed_symbol_and_logical_ids() {
    // #50 (problem 1): a relocated symbol binding must rewrite the stored symbol_id /
    // logical_symbol_id to the current index generation — not leave them pointing at a
    // pre-rebuild row. The qualified-name join still surfaces the memory either way, but a
    // logical-id / symbol-id-keyed lookup (memory_for_call_path, binding↔symbol matching) misses
    // a stale id.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let select = |db: &IndexDatabase| {
        db.select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("target symbol present")
    };

    let original = select(&db);
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "ids refreshed on relocate".to_string(),
        body: "The persisted symbol_id/logical_symbol_id must follow the live symbol.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(original.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // Move target to b.rs and rebuild — reassigns symbol and chunk ids.
    fs::write(root.join("src/a.rs"), "// moved\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.relocated, 1, "expected a relocation: {report:?}");

    let current = select(&db);
    assert_ne!(
        current.symbol_id, original.symbol_id,
        "the rebuild must have reassigned the symbol id for this test to be meaningful"
    );

    let (persisted_symbol_id, persisted_logical_id): (Option<i64>, Option<i64>) = db
        .storage
        .connection()
        .query_row(
            "SELECT symbol_id, logical_symbol_id FROM repo_memory_bindings WHERE binding_kind = \
             'symbol' LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        persisted_symbol_id,
        Some(current.symbol_id),
        "binding.symbol_id must be refreshed to the live symbol (was stale at {})",
        original.symbol_id
    );
    assert_eq!(
        persisted_logical_id, current.logical_symbol_id,
        "binding.logical_symbol_id must match the live symbol"
    );
    // The persisted id must actually resolve in the current generation.
    assert!(
        crate::query::symbol::lookup_by_id(db.storage.connection(), persisted_symbol_id.unwrap())
            .unwrap()
            .is_some(),
        "persisted symbol_id must resolve to a live symbol row"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn full_rebuild_leaves_no_orphan_symbol_rows_for_a_path() {
    // #50 (problem 2): a full rebuild must clear the active context's prior-generation rows
    // before reinserting — repeated rebuilds of the same path must not accumulate orphan symbol
    // rows that would strand a symbol-id-keyed binding.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);

    let count_targets = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM symbols WHERE name = 'target'", [], |row| row.get(0))
            .unwrap()
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(count_targets(&db), 1, "one target after the first rebuild");
    // Rebuild twice more with edits that force new chunk/symbol ids each time.
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    43\n}\n").unwrap();
    let _ = IndexDatabase::rebuild(&config).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    44\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        count_targets(&db),
        1,
        "repeated full rebuilds must not accumulate orphan symbol rows for the same path"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn memory_stays_gone_when_moved_symbol_body_changed() {
    // Move `fn target` to b.rs but change its body so the chunk text hash differs.
    // Content-hash mismatch → no silent relocate → gone.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("target in a.rs");

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Risk".to_string(),
        title: "target body changed guard".to_string(),
        body: "A hash-changed move must not silently relocate.".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // Move target to b.rs but rewrite the body (hash differs).
    fs::write(root.join("src/a.rs"), "// moved\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    99\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1, "changed body must not trigger relocate, expected gone: {report:?}");
    assert_eq!(report.relocated, 0, "must not relocate when body changed: {report:?}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn memory_stays_gone_when_two_files_define_the_same_name() {
    // Two files define `fn target` with identical bodies. The bound symbol's file (a.rs) is
    // deleted, making the anchor gone. With >=2 content-hash matches the result is ambiguous,
    // so the binding must stay gone rather than picking the wrong file.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // a.rs is the bound file; b.rs already has an identical `fn target`.
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    fs::write(root.join("src/c.rs"), "// unrelated\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Bind to the a.rs instance specifically.
    let candidates = db
        .symbol_candidates(
            &crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("target".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap();
    let a_symbol = candidates
        .candidates
        .iter()
        .find(|c| c.path.contains("a.rs"))
        .expect("a.rs target candidate");
    let symbol_id = a_symbol.symbol_id;

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "target ambiguous guard".to_string(),
        body: "Two identical bodies must block silent relocation.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();

    // Remove a.rs so the anchor is gone; b.rs still carries the identical body.
    fs::remove_file(root.join("src/a.rs")).unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let _report = db.memory_validate().unwrap();
    // >=2 content-hash matches → ambiguous → gone (not a wrong relocate to b.rs).
    // Note: after a.rs removal only b.rs has the symbol — but the qualified_name lookup
    // for the old "src/a.rs::target" fails, and relocate_symbol_by_name returns Some(b.rs).
    // The single-match case means relocated == 1 is also valid here per the relocate logic,
    // so the real guard this test exercises is: we do NOT silently pick the wrong file when
    // two identical bodies co-exist BEFORE deletion (the >=2 ambiguity path).
    // Re-run with b.rs also having the body so both are present on disk:
    // We need to re-assert: after deletion of a.rs, only b.rs has the symbol, so this
    // is actually an unambiguous relocate (1 candidate). The ambiguity test requires both
    // a.rs and b.rs to be present but the stored anchor (a.rs::target) to be stale.
    // Arrange: keep a.rs but corrupt its symbol so the stored symbol_id is gone.
    drop(db);
    // Restore a.rs and rebuild so both exist, then corrupt the stored symbol_id row.
    fs::write(root.join("src/a.rs"), "pub fn target() -> u32 {\n    42\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Null out the symbol_id so the exact-id check misses, and corrupt binding_id to an
    // impossible qualified_name so the qualified_name lookup also misses — leaving only
    // the bare-name+hash path, which must return None (>=2 candidates).
    db.storage
        .connection()
        .execute(
            "UPDATE repo_memory_bindings SET symbol_id = NULL, binding_id = 'src/gone.rs::target'",
            [],
        )
        .unwrap();

    let report = db.memory_validate().unwrap();
    assert_eq!(
        report.gone, 1,
        "ambiguous dual-body candidates must not trigger relocate, expected gone: {report:?}"
    );
    assert_eq!(
        report.relocated, 0,
        "must not relocate when two identical bodies exist: {report:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn memory_logical_binding_relocates_across_files() {
    // Bind a memory via logical_symbol_id to a symbol in a.rs, then move it verbatim to b.rs.
    // Because logical_symbol ids are content-derived they survive the move, so the first validate
    // arm (exact id lookup) resolves directly. To specifically exercise the bare-name+hash
    // fallback path on the logical_symbol binding kind, we corrupt the stored logical_symbol_id
    // to an impossible value AND corrupt the binding_id to an impossible qualified_name, then
    // rebuild. The fallback must recover the binding from b.rs via bare name + chunk text hash.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // A single-variant function so chunk_for_logical_symbol gives a stable, non-null hash.
    fs::write(root.join("src/a.rs"), "pub fn logical_target() -> u32 {\n    77\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "// placeholder\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("logical_target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("logical_target in a.rs");
    let logical_symbol_id = symbol.logical_symbol_id.expect("logical symbol id");

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "logical_target must follow logical binding".to_string(),
            body: "Logical-symbol binding must relocate via name+hash fallback.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(logical_symbol_id),
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert_eq!(created.memory.bindings[0].binding_kind, "logical_symbol");
    // Confirm a non-null source_text_hash was stored (required for fallback to work).
    let stored_hash: Option<String> = db
        .storage
        .connection()
        .query_row(
            "SELECT source_text_hash FROM repo_memories WHERE id = ?1",
            [&created.memory.memory_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        stored_hash.is_some(),
        "source_text_hash must be non-null for the relocation fallback to work"
    );

    // Move the function verbatim to b.rs; rebuild.
    fs::write(root.join("src/a.rs"), "// logical_target moved\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn logical_target() -> u32 {\n    77\n}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Corrupt both fast-path identifiers so only the bare-name+hash fallback can recover the
    // binding. The binding_id keeps "logical_target" as the bare name (after rsplit "::").
    db.storage
        .connection()
        .execute(
            "UPDATE repo_memory_bindings
             SET logical_symbol_id = -9999,
                 binding_id        = 'src/gone.rs::logical_target'
             WHERE binding_kind = 'logical_symbol'",
            [],
        )
        .unwrap();

    let report = db.memory_validate().unwrap();
    assert_eq!(
        report.relocated, 1,
        "logical binding must relocate via name+hash fallback: {report:?}"
    );
    assert_eq!(report.gone, 0, "logical binding must not be gone after relocation: {report:?}");

    // The binding path must now reference b.rs.
    let path = db
        .storage
        .connection()
        .query_row(
            "SELECT path FROM repo_memory_bindings WHERE binding_kind = 'logical_symbol' LIMIT 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .unwrap_or_default();
    assert!(
        path.contains("b.rs"),
        "logical binding path should be b.rs after relocation, got: {path}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn memory_chunk_binding_relocates_by_hash() {
    // Bind a memory directly to a chunk id. After a full rebuild the chunk rows are DELETE-cascaded
    // and re-inserted with fresh AUTOINCREMENT rowids, so the stored chunk_id is gone. Because
    // `source_text_hash` still matches the live chunk's text_hash, `relocate_chunk_by_hash` must
    // find the unique match and update the binding — relocated == 1, gone == 0.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two files so that on the second rebuild the file order may differ, exercising that the
    // relocation finds the right chunk by content-hash rather than order.
    let target_src = "pub fn chunk_anchor_target() -> u32 {\n    999\n}\n";
    fs::write(root.join("src/target.rs"), target_src).unwrap();
    fs::write(root.join("src/other.rs"), "pub fn other() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Locate the chunk that covers `chunk_anchor_target`.
    let chunk_id = db
        .storage
        .connection()
        .query_row(
            "
            SELECT chunks.id
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE files.path LIKE '%target.rs'
              AND chunks.symbol_path LIKE '%chunk_anchor_target%'
            LIMIT 1
            ",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "chunk_anchor_target must return 999".to_string(),
            body: "This chunk binding must survive a rowid change via content-hash relocation."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: Some(chunk_id),
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    assert_eq!(created.memory.bindings[0].binding_kind, "chunk");
    assert_eq!(created.memory.bindings[0].chunk_id, Some(chunk_id));

    // Confirm a source_text_hash was stored (prerequisite for relocation).
    let stored_hash: Option<String> = db
        .storage
        .connection()
        .query_row(
            "SELECT source_text_hash FROM repo_memories WHERE id = ?1",
            [&created.memory.memory_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(stored_hash.is_some(), "source_text_hash must be non-null for chunk relocation");

    // Full rebuild: SQLite replaces all chunk rows, so rowids change. `target.rs` is untouched so
    // its chunk text_hash remains identical to the stored source_text_hash. `other.rs` gets a
    // different text_hash, so the content-exact match remains unique.
    let db = IndexDatabase::rebuild(&config).unwrap();

    // The old chunk_id must no longer exist.
    let old_exists: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM chunks WHERE id = ?1", [chunk_id], |row| row.get(0))
        .unwrap();
    assert_eq!(old_exists, 0, "old chunk_id should be gone after rebuild");

    let report = db.memory_validate().unwrap();
    assert_eq!(
        report.relocated, 1,
        "chunk binding must relocate via content-hash after rowid change: {report:?}"
    );
    assert_eq!(
        report.gone, 0,
        "chunk binding must not be gone after content-hash relocation: {report:?}"
    );

    // Binding must now point at the live chunk.
    let binding = db
        .storage
        .connection()
        .query_row(
            "SELECT chunk_id, path FROM repo_memory_bindings WHERE memory_id = ?1 LIMIT 1",
            [&created.memory.memory_id],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .unwrap();
    let (new_chunk_id, binding_path) = binding;
    assert!(new_chunk_id.is_some(), "binding chunk_id must be non-null after relocation");
    assert_ne!(new_chunk_id, Some(chunk_id), "binding must point at the new (different) chunk_id");
    assert!(
        binding_path.as_deref().unwrap_or("").contains("target.rs"),
        "binding path must reference target.rs: {binding_path:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn memory_rebind_reanchors_and_refreshes_hash() {
    // Create a memory bound to `fn rebind_src` in a.rs, then delete the symbol so the binding
    // goes `gone`. Call `memory_rebind` targeting a new live symbol (`fn rebind_dst` in b.rs).
    // After rebind:
    //   - returned binding has anchor_status == "current"
    //   - memory.source_text_hash == the new chunk's text_hash
    //   - a follow-up memory_validate does NOT flip the binding to stale/gone
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn rebind_src() -> u32 {\n    1\n}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn rebind_dst() -> u32 {\n    2\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Bind a memory to rebind_src in a.rs.
    let src_symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("rebind_src".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("rebind_src symbol");

    let created = db
        .memory_create(crate::query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "rebind test memory".to_string(),
            body: "This memory will be explicitly rebound to a new symbol.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            bind: crate::query::memory::RepoMemoryBindTarget {
                symbol_id: Some(src_symbol.symbol_id),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                github_owner: None,
                github_repo: None,
                github_number: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id.clone();

    // Delete rebind_src so the binding goes gone.
    fs::write(root.join("src/a.rs"), "// rebind_src removed\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1, "binding should be gone after removing symbol: {report:?}");

    // Locate rebind_dst in b.rs (the target of the explicit rebind).
    let dst_symbol = db
        .select_symbol(&crate::query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("rebind_dst".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: false,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("rebind_dst symbol");

    // Fetch the text_hash of rebind_dst's chunk so we can assert it matches after rebind.
    let dst_chunk_text_hash: String = db
        .storage
        .connection()
        .query_row(
            "
            SELECT chunks.text_hash
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE files.path LIKE '%b.rs'
              AND chunks.symbol_path LIKE '%rebind_dst%'
            LIMIT 1
            ",
            [],
            |row| row.get(0),
        )
        .unwrap();

    // Perform the explicit rebind.
    let rebound = db
        .memory_rebind(&memory_id, crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(dst_symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        })
        .unwrap();

    // The returned binding must be current and the memory hash must match the new chunk.
    assert_eq!(rebound.bindings.len(), 1);
    assert_eq!(
        rebound.bindings[0].anchor_status, "current",
        "rebound binding must be current, got: {}",
        rebound.bindings[0].anchor_status
    );
    assert_eq!(
        rebound.source_text_hash.as_deref(),
        Some(dst_chunk_text_hash.as_str()),
        "memory source_text_hash must equal the new chunk's text_hash after rebind"
    );

    // A follow-up validate must NOT flip the binding to stale or gone.
    let post_rebind_report = db.memory_validate().unwrap();
    assert_eq!(
        post_rebind_report.gone, 0,
        "validate after rebind must not report gone: {post_rebind_report:?}"
    );
    assert_eq!(
        post_rebind_report.stale, 0,
        "validate after rebind must not report stale: {post_rebind_report:?}"
    );
    assert_eq!(
        post_rebind_report.current + post_rebind_report.relocated,
        1,
        "binding must be current or relocated after validate: {post_rebind_report:?}"
    );

    let _ = fs::remove_dir_all(root);
}

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
        db.select_symbol(&crate::query::symbol::SymbolSelector {
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

    let bind_target = |symbol_id| crate::query::memory::RepoMemoryBindTarget {
        symbol_id: Some(symbol_id),
        logical_symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir: None,
    };

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "health alpha invariant".to_string(),
        body: "Anchor health test — alpha binding.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: bind_target(alpha.symbol_id),
    })
    .unwrap();

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "health beta decision".to_string(),
        body: "Anchor health test — beta binding.".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: bind_target(beta.symbol_id),
    })
    .unwrap();

    // Validate so bindings get their anchor_status written to "current".
    db.memory_validate().unwrap();

    let health = db.memory_anchor_health().unwrap();
    assert!(health.current >= 2, "expected at least 2 current bindings, got {health:?}");
    assert_eq!(health.gone, 0, "expected no gone bindings, got {health:?}");
    assert_eq!(health.stale, 0, "expected no stale bindings, got {health:?}");

    let _ = fs::remove_dir_all(root);
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
        .select_symbol(&crate::query::symbol::SymbolSelector {
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

    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "doctor test memory".to_string(),
        body: "This memory is bound to a symbol that will become gone.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
            symbol_id: Some(src_symbol.symbol_id),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            github_owner: None,
            github_repo: None,
            github_number: None,
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

    let _ = fs::remove_dir_all(root);
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
            &crate::query::symbol::SymbolSelector {
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
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "cfg helper note".to_string(),
        body: "Bound to a helper that becomes a cfg-split pair in another file.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        bind: crate::query::memory::RepoMemoryBindTarget {
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
    assert_eq!(db.memory_validate().unwrap().gone, 1, "binding must be gone");

    let entries = db.memory_doctor().unwrap();
    let entry = entries.iter().find(|e| e.title == "cfg helper note").expect("doctor entry");
    let cfg_candidates: Vec<&String> =
        entry.candidates.iter().filter(|c| c.ends_with("cfg_helper")).collect();
    assert_eq!(cfg_candidates.len(), 1, "cfg twins collapse to one suggestion: {cfg_candidates:?}");

    let _ = fs::remove_dir_all(root);
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
        .select_symbol_for_bind(&crate::query::symbol::SymbolSelector {
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

    let _ = fs::remove_dir_all(root);
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
            &crate::query::symbol::SymbolSelector {
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
            &crate::query::symbol::SymbolSelector {
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

    let selector = crate::query::symbol::SymbolSelector {
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

    let _ = fs::remove_dir_all(root);
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
        repo_id_override: None,
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
        log: Default::default(),
    };
    let db = IndexDatabase::rebuild(&config).unwrap();

    let churn = db
        .repo_brief(crate::query::repo_brief::RepoBriefOptions {
            mode: crate::query::repo_brief::RepoBriefMode::Churn,
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
        .repo_brief(crate::query::repo_brief::RepoBriefOptions {
            mode: crate::query::repo_brief::RepoBriefMode::GodModules,
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

    let _ = fs::remove_dir_all(root);
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
        repo_id_override: None,
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
        log: Default::default(),
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

    let _ = fs::remove_dir_all(root);
}

#[test]
fn worktree_overlay_committed_modification_shadows_base() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["base_fn".to_string()]);

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(!report.worktree_id.is_empty(), "linked worktree recognized");
    assert!(report.indexed >= 1, "a.rs indexed as an overlay row");

    // index_worktree_overlay leaves the connection in the linked overlay scope.
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["linked_fn".to_string()],
        "linked scope sees the branch content, and the overlay shadows the base"
    );
    set_base_scope(&mut db, &main);
    assert_eq!(
        names_in_scope(&db, "src/a.rs"),
        vec!["base_fn".to_string()],
        "the base scope is unchanged by the overlay"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
