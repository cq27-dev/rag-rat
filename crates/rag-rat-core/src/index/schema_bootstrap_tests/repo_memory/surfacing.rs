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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Treat cfg helper variants as one logical helper".to_string(),
            body: "Caller and impact analysis should use the logical symbol, not one cfg body \
                   variant."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["cfg".to_string(), "graph".to_string()],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(logical_symbol_id),
                symbol_id: None,
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
    assert!(!created.duplicate);
    assert_eq!(created.memory.bindings[0].binding_kind, "logical_symbol");

    let memories =
        db.memory_for_symbol(&symbol, 10, rag_rat_base::config::MemorySurface::Full).unwrap();
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
            &rag_rat_query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    assert_eq!(impact.repo_memories.compact().unwrap().direct.len(), 1);
    assert_eq!(impact.completeness_and_caveats.memory_status.active, 1);
    assert_eq!(impact.completeness_and_caveats.memory_status.stale, 0);

    let _ = fs::remove_dir_all(&root);
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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Runtime shutdown must be idempotent".to_string(),
            body: full_body.clone(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["runtime".to_string()],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
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
            &rag_rat_query::impact::ImpactSurfaceOptions::default(),
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
            &rag_rat_query::impact::ImpactSurfaceOptions {
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

    let _ = fs::remove_dir_all(&root);
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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchor drifts when the chunk hash changes".to_string(),
            body: "Stale anchors belong in their own lane, away from current evidence.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
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
            &rag_rat_query::impact::ImpactSurfaceOptions::default(),
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
            &rag_rat_query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();
    let after = after.repo_memories.compact().unwrap();
    assert!(after.direct.is_empty(), "a stale memory leaves the direct lane");
    assert_eq!(after.stale.len(), 1);
    assert_eq!(after.stale[0].memory_id, created.memory.memory_id);
    assert_eq!(after.stale[0].anchor_status.as_deref(), Some("stale"));

    let _ = fs::remove_dir_all(&root);
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

    let selector = rag_rat_query::symbol::SymbolSelector {
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "keystone holds an invariant".to_string(),
            body: "This memory must survive a reindex and follow the symbol when it moves."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                symbol_id: Some(symbol.symbol_id),
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

    // Edit the file so keystone moves down (new symbol ids on reindex), then rebuild.
    fs::write(root.join("src/lib.rs"), "pub fn added_above() {}\n\npub fn keystone() {}\n")
        .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Memory row survives the reindex (no cascade from deleted symbols).
    assert!(
        rag_rat_query::memory::memory_by_id(db.storage.connection(), &created.memory.memory_id,)
            .unwrap()
            .is_some(),
        "memory was lost to reindex",
    );

    // Re-validation re-anchors the binding to keystone's new location, not "gone".
    db.memory_validate().unwrap();
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol after move");
    let anchored =
        db.memory_for_symbol(&symbol, 10, rag_rat_base::config::MemorySurface::Full).unwrap();
    assert_eq!(anchored.len(), 1, "memory did not re-anchor to moved symbol");
    assert_ne!(anchored[0].bindings[0].anchor_status, "gone");

    let _ = fs::remove_dir_all(&root);
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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchor must become stale when source hash changes".to_string(),
            body: "Validation should separate stale memories from current repo evidence."
                .to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: Some(chunk_id),
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

    db.storage
        .connection()
        .execute("UPDATE chunks SET text_hash = 'changed' WHERE id = ?1", [chunk_id])
        .unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.stale, 1);
    let stale =
        db.memory_for_symbol(&symbol, 10, rag_rat_base::config::MemorySurface::Full).unwrap();
    assert_eq!(stale[0].memory_id, created.memory.memory_id);
    assert_eq!(stale[0].bindings[0].anchor_status, "stale");

    db.storage.connection().execute("DELETE FROM chunks WHERE id = ?1", [chunk_id]).unwrap();
    // Twice: the #492 downgrade hysteresis defers the first gone observation.
    db.memory_validate().unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1);
    let gone =
        db.memory_for_symbol(&symbol, 10, rag_rat_base::config::MemorySurface::Full).unwrap();
    assert_eq!(gone[0].bindings[0].anchor_status, "gone");

    let _ = fs::remove_dir_all(&root);
}

/// #491: one qualified name, two logical twins (a `struct` and its `impl` block). The impl's
/// logical key sorts first ("impl" < "struct"), so it gets the lower rowid and an unordered
/// `LIMIT 1` relocation deterministically lands a struct-bound memory on the impl row. The
/// binding stores the V014 discriminators (`symbol_kind`, `signature_hash`) for exactly this —
/// relocation must prefer the twin that agrees with them.
#[test]
fn relocation_lands_on_the_kind_matching_twin_not_plan_order() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/twin.rs"),
        "pub struct TwinAnchor {\n    pub x: i64,\n}\n\nimpl TwinAnchor {\n    pub fn \
         probe(&self) -> i64 {\n        self.x\n    }\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let twin_id = |kind: &str| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT ls.id FROM logical_symbols ls
                 JOIN name_strings qn ON qn.id = ls.qualified_name_id
                 WHERE qn.value LIKE '%::TwinAnchor' AND ls.kind = ?1",
                [kind],
                |r| r.get(0),
            )
            .unwrap()
    };
    let struct_id = twin_id("struct");
    let impl_id = twin_id("impl");
    assert_ne!(struct_id, impl_id, "the fixture must produce both twins");

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Bound to the struct twin".to_string(),
            body: "Relocation must land back on the struct, not the impl block.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(struct_id),
                symbol_id: None,
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
    let memory_id = created.memory.memory_id;

    // Simulate key-derivation drift: the stored id no longer resolves, while the qualified name
    // and the stored discriminators (symbol_kind = struct + the struct's signature hash) survive.
    db.storage
        .connection()
        .execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = 424242 WHERE memory_id = ?1",
            params![memory_id],
        )
        .unwrap();

    db.memory_validate().unwrap();
    let (relocated_id, status): (i64, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id, anchor_status FROM repo_memory_bindings
             WHERE memory_id = ?1",
            params![memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "relocated", "the dead id must relocate by qualified name");
    assert_eq!(
        relocated_id, struct_id,
        "relocation must prefer the kind-matching twin (struct), not the impl row plan order \
         happens to return first"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #492: the path probe took the newest `files` row with no `kind != 'deleted'` filter, and a
/// deleted-at-HEAD file's marker row (kind='deleted', sha256='') shadowed the absence — a bare
/// path binding to a deleted file stayed `current` forever.
#[test]
fn a_deleted_at_head_file_makes_a_bare_path_binding_gone() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keeper() {}\n").unwrap();
    fs::write(root.join("src/doomed.rs"), "pub fn doomed() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Area note on a file that will be deleted".to_string(),
            body: "Must go gone with its file, not stay current behind the deleted marker."
                .to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/doomed.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id;
    let status = |db: &IndexDatabase| -> String {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status FROM repo_memory_bindings WHERE memory_id = ?1",
                params![memory_id],
                |r| r.get(0),
            )
            .unwrap()
    };
    db.memory_validate().unwrap();
    assert_eq!(status(&db), "current", "alive file → current");

    // The file is deleted at HEAD: the index keeps a deleted-marker row (kind='deleted',
    // sha256=''), exactly the shape the incremental delete path leaves behind.
    fs::remove_file(root.join("src/doomed.rs")).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'src/doomed.rs'",
            [],
        )
        .unwrap();
    // Twice: the #492 downgrade hysteresis defers the first gone observation.
    db.memory_validate().unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(status(&db), "gone", "the deleted marker must not shadow the file's absence");
    assert_eq!(report.gone, 1);

    let _ = fs::remove_dir_all(&root);
}

/// #492: a path that is absent at THIS context's HEAD but alive in another indexed scope (a
/// linked-worktree overlay — an in-flight branch) is `pending`, not `gone`. Verified live on the
/// dogfood DB: a forward anchor to a branch-only file ping-ponged current/gone between contexts,
/// and doctor advised mark-obsolete for valid in-flight work.
#[test]
fn a_path_alive_only_in_another_scope_validates_pending_not_gone() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn base() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let live_generation =
        rag_rat_db::schema::live_files_generation(db.storage.connection(), &db.active_repo_id)
            .unwrap();
    let insert_row = |generation: i64, sha: &str| {
        db.storage
            .connection()
            .execute(
                "INSERT INTO main.files (path, language, kind, sha256, modified_at_ms,
                                    generated, indexed_at_ms, indexed_revision, commit_sha,
                                    worktree_id, has_test_code, repo_id, generation)
                 VALUES ('src/inflight.rs', 'rust', 'source', ?2, 0, 0, 0, '', '',
                         'wt-elsewhere', 0, ?1, ?3)",
                params![db.active_repo_id, sha, generation],
            )
            .unwrap();
    };

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "Forward anchor to in-flight work".to_string(),
            body: "Absent here, alive on a branch — pending, never mark-obsolete bait.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/inflight.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id;
    let status = |db: &IndexDatabase| -> String {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status FROM repo_memory_bindings WHERE memory_id = ?1",
                params![memory_id],
                |r| r.get(0),
            )
            .unwrap()
    };

    // An ABANDONED staging's row (generation above live) is NOT alive-elsewhere evidence — only
    // live-generation rows (worktree overlays ride the live generation) count. It IS, however, a
    // torn window for the #492 downgrade hysteresis: while it exists, the observed gone neither
    // arms nor confirms, so the persisted status holds.
    insert_row(live_generation + 7, "dead-gen-sha");
    let report = db.memory_validate().unwrap();
    assert_eq!(report.pending, 0, "a staged generation's leftover row must not report pending");
    assert_eq!(report.gone, 1, "the observation is gone, not pending");
    assert_eq!(status(&db), "current", "a staged-window observation must not move the status");

    // gc sweeps the abandoned staging: the two-pass downgrade proceeds.
    db.storage
        .connection()
        .execute("DELETE FROM main.files WHERE sha256 = 'dead-gen-sha'", [])
        .unwrap();
    db.memory_validate().unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(status(&db), "gone", "two trustworthy observations persist the downgrade");
    assert_eq!(report.pending, 0);

    // A retained old-commit BASE row (worktree_id = '') at the live generation is THIS context's
    // history, not another checkout — a path deleted at HEAD must stay gone through the pre-gc
    // window (review case 2).
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files (path, language, kind, sha256, modified_at_ms, generated,
                                indexed_at_ms, indexed_revision, commit_sha, worktree_id,
                                has_test_code, repo_id, generation)
             VALUES ('src/inflight.rs', 'rust', 'source', 'old-commit-sha', 0, 0, 0, '',
                     'oldcommit', '', 0, ?1, ?2)",
            params![db.active_repo_id, live_generation],
        )
        .unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(status(&db), "gone", "a retained old-commit base row must not report pending");
    assert_eq!(report.pending, 0);

    // The in-flight branch's checkout indexed the file at the LIVE generation (the
    // worktree-overlay shape) — now it is genuinely pending.
    insert_row(live_generation, "wt-sha");
    let report = db.memory_validate().unwrap();
    assert_eq!(status(&db), "pending", "alive in another scope → pending, not gone");
    assert_eq!(report.pending, 1);
    assert_eq!(report.gone, 0);

    // Doctor surfaces it (as pending — informational), never as gone.
    let doctor = db.memory_doctor().unwrap();
    let entry = doctor.iter().find(|e| e.memory_id == memory_id).expect("doctor lists pending");
    assert_eq!(entry.anchor_status, "pending");

    // Once no scope holds the path (the branch was abandoned), it is genuinely gone.
    db.storage
        .connection()
        .execute("DELETE FROM main.files WHERE path = 'src/inflight.rs'", [])
        .unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.pending, 0);
    assert_eq!(report.gone, 1);

    let _ = fs::remove_dir_all(&root);
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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
    let graph_options = rag_rat_query::graph::GraphTraversalOptions {
        resolution_mode: rag_rat_query::graph::GraphResolutionMode::Exact,
        symbol_id: Some(target.symbol_id),
        logical_symbol_id: target.logical_symbol_id,
        ..Default::default()
    };
    let callers =
        db.graph_traversal_report("find_callers", &target, true, 10, &graph_options).unwrap();
    let edge_id = callers.results[0].edge_id;

    let edge_memory = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "caller_edge to target_edge must stay synchronous".to_string(),
            body: "This specific call path is used to prove edge-bound memories surface when \
                   impact crosses the edge."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["edge".to_string()],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: Some(edge_id),
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
    assert_eq!(edge_memory.memory.bindings[0].binding_kind, "edge");
    assert_eq!(edge_memory.memory.bindings[0].edge_id, Some(edge_id));

    let impact = db
        .impact_surface_report_for_selected_symbol(
            &target,
            10,
            &rag_rat_query::impact::ImpactSurfaceOptions {
                resolution_mode: rag_rat_query::graph::GraphResolutionMode::Exact,
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "TestExpectation".to_string(),
            title: "caller_edge path hash recall".to_string(),
            body: "Call-path memories are addressable by a deterministic edge sequence hash."
                .to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec!["call-path".to_string()],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                tracker: None,
                project: None,
                item_key: None,
                start_logical_symbol_id: target.logical_symbol_id,
                end_logical_symbol_id: target.logical_symbol_id,
                edge_sequence_hash: Some("edge-sequence-test-hash".to_string()),
                path_summary: Some("caller_edge -> target_edge".to_string()),
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
    let call_path = db
        .memory_for_call_path_hash(
            "edge-sequence-test-hash",
            10,
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap();
    assert_eq!(call_path.len(), 1);
    assert_eq!(call_path[0].memory_id, call_path_memory.memory.memory_id);
    assert_eq!(call_path[0].call_paths[0].path_summary, "caller_edge -> target_edge");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn memory_search_defers_the_body_under_the_summary_surface() {
    // #5: `memory_search` now honors `[memory] surface` (summary by default). Under `Summary` the
    // full body is deferred to `memory show` (title-only when no dream summary row exists yet);
    // under `Full` the whole body is returned.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "surfaceprobe invariant".to_string(),
        body: "The surfaceprobe body is load-bearing and worth compacting.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: Some("src/lib.rs".to_string()),
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

    let full =
        db.memory_search("surfaceprobe", 10, rag_rat_base::config::MemorySurface::Full).unwrap();
    assert_eq!(full.len(), 1, "the memory is found");
    assert!(!full[0].body.is_empty(), "the `full` surface returns the whole body");

    let summary =
        db.memory_search("surfaceprobe", 10, rag_rat_base::config::MemorySurface::Summary).unwrap();
    assert_eq!(summary.len(), 1, "the same hit under the default surface");
    assert_eq!(summary[0].memory_id, full[0].memory_id);
    assert!(
        summary[0].body.is_empty(),
        "the `summary` surface defers the body to `memory show` (title-only with no summary row \
         yet)"
    );

    let _ = fs::remove_dir_all(&root);
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

    // A6: scope the edge lookup through the live-generation `files` view (`JOIN files` on
    // `source_file_id`). The rebuild stages a fresh generation and leaves the prior generation's
    // `edges_data` rows in place (dead until gc), so a raw `edges` read with `ORDER BY id LIMIT 1`
    // would keep returning the dead gen's edge — this pins the LIVE edge id, which the rebuild does
    // reassign.
    let edge_id = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT edges.id FROM edges JOIN files ON files.id = edges.source_file_id
                 WHERE edges.to_name LIKE '%callee%' ORDER BY edges.id LIMIT 1",
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "why callee is invoked here".to_string(),
            body: "This call path is load-bearing.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: None,
                symbol_id: None,
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
    let found =
        db.memory_for_call_path_hash(&hash, 10, rag_rat_base::config::MemorySurface::Full).unwrap();
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
    assert_eq!(
        db.memory_for_call_path_hash(&hash, 10, rag_rat_base::config::MemorySurface::Full)
            .unwrap()
            .len(),
        1
    );

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

    // Remove the call site → the edge is gone → the call path is gone. Two passes: the #492
    // downgrade hysteresis defers the first gone observation.
    fs::write(root.join("src/lib.rs"), "pub fn caller() {}\npub fn callee() {}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.memory_validate().unwrap();
    db.memory_validate().unwrap();
    assert_eq!(call_path_status(&db), "gone", "deleting the call site makes the path gone");

    let _ = fs::remove_dir_all(&root);
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

    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "a -> b -> c is the hot path".to_string(),
        body: "Why this two-hop path matters.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            logical_symbol_id: None,
            symbol_id: None,
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
            edge_path: Some(vec![caller_edge, callee_edge]),
            dir: None,
        },
    })
    .unwrap();

    let symbol_b = db
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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

    let report = rag_rat_query::impact::impact_surface_report_for_symbol(
        db.storage.connection(),
        &symbol_b,
        10,
        &rag_rat_query::impact::ImpactSurfaceOptions::default(),
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

    let _ = fs::remove_dir_all(&root);
}
