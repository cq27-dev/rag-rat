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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "target returns 42".to_string(),
            body: "This memory must follow target across a file move.".to_string(),
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
            &db.select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
            rag_rat_base::config::MemorySurface::Full,
        )
        .unwrap()[0]
        .bindings[0]
        .clone();
    let path = binding.path.as_deref().unwrap_or("");
    assert!(path.contains("b.rs"), "binding path should be b.rs after relocation: {path}");
    assert_ne!(binding.anchor_status, "gone");

    // Post-condition: memory validation/relocation (which rewrites bindings) must not rebind onto,
    // or delete, a SIBLING repo's rows (round-6 harness; guards the P2 #2 relocation scoping).
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());
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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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

    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "target durable across reindex".to_string(),
        body: "After relocation the binding must stay stable on a second reindex.".to_string(),
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
        db.select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "ids refreshed on relocate".to_string(),
        body: "The persisted symbol_id/logical_symbol_id must follow the live symbol.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            symbol_id: Some(original.symbol_id),
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
        rag_rat_query::symbol::lookup_by_id(db.storage.connection(), persisted_symbol_id.unwrap())
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

    // A6: count through the scope VIEW (`files` = the live-generation `temp.files`). A full rebuild
    // stages a fresh generation and leaves the prior one's symbol rows in RAW `main.symbols` (dead,
    // reclaimed lazily by gc — which is a no-op on this non-git fixture, so they persist here). But
    // a reader and a symbol-id-keyed binding only ever see the LIVE generation, which is
    // exactly one `target` — the #50 invariant restated for generation staging (a stranded
    // binding relocates against the live generation, never onto a dead orphan).
    let count_targets = |db: &IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM symbols JOIN files ON files.id = symbols.file_id
                 WHERE symbols.name = 'target'",
                [],
                |row| row.get(0),
            )
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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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

    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Risk".to_string(),
        title: "target body changed guard".to_string(),
        body: "A hash-changed move must not silently relocate.".to_string(),
        confidence: "medium".to_string(),
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
            &rag_rat_query::symbol::SymbolSelector {
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

    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "target ambiguous guard".to_string(),
        body: "Two identical bodies must block silent relocation.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
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
    // the bare-name+hash path, which must return None (>=2 candidates). Scoped to the test's
    // own SYMBOL binding: the poison sibling seeds two PATH bindings under one memory, and an
    // unscoped rewrite would collapse their binding_ids into a PK collision.
    db.storage
        .connection()
        .execute(
            "UPDATE repo_memory_bindings SET symbol_id = NULL, binding_id = 'src/gone.rs::target' \
             WHERE binding_kind = 'symbol'",
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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "logical_target must follow logical binding".to_string(),
            body: "Logical-symbol binding must relocate via name+hash fallback.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "chunk_anchor_target must return 999".to_string(),
            body: "This chunk binding must survive a rowid change via content-hash relocation."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
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

    // The old chunk_id must no longer refer to a LIVE chunk. A6: the rebuild stages a fresh
    // generation and leaves the old chunk row in `main.chunks` (dead until gc), so scope through
    // the live-generation `files` view — the dead chunk's file row is absent from it, so the
    // join drops the stale rowid exactly as a reader (and `relocate_chunk_by_hash`) sees it.
    let old_exists: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM chunks JOIN files ON files.id = chunks.file_id
             WHERE chunks.id = ?1",
            [chunk_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_exists, 0, "old chunk_id should no longer refer to a live chunk after rebuild");

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
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "rebind test memory".to_string(),
            body: "This memory will be explicitly rebound to a new symbol.".to_string(),
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
    let memory_id = created.memory.memory_id.clone();

    // Delete rebind_src so the binding goes gone.
    fs::write(root.join("src/a.rs"), "// rebind_src removed\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1, "binding should be gone after removing symbol: {report:?}");

    // Locate rebind_dst in b.rs (the target of the explicit rebind).
    let dst_symbol = db
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
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
        .memory_rebind(&memory_id, rag_rat_query::memory::RepoMemoryBindTarget {
            symbol_id: Some(dst_symbol.symbol_id),
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

    let _ = fs::remove_dir_all(root);
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
        trackers: Vec::new(),
        papertrail: Default::default(),
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
        trackers: Vec::new(),
        papertrail: Default::default(),
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

#[test]
fn surface_summary_defers_bodies_across_the_db_memory_renderers() {
    // #426: memory_for_symbol / memory_for_path / read_chunk / memory_evidence_for_symbol_and_edges
    // all honor `[memory] surface = "summary"` — the full body is deferred to `memory show`, the
    // compacted summary + verdict marker take its place, and the binding structure is preserved.
    use rag_rat_base::config::MemorySurface;
    use rag_rat_query::graph_meta::GraphMetaMode;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn target() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db
        .select_symbol(&rag_rat_query::symbol::SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: None,
            symbol: Some("target".to_string()),
            language: Some(Language::Rust),
            allow_ambiguous: true,
            limit: 10,
        })
        .unwrap()
        .unwrap()
        .expect("selected symbol");

    let sym_title = "Target invariant";
    let sym_body = "The target function must keep its full documented invariant intact.";
    let sym_mem = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: sym_title.to_string(),
            body: sym_body.to_string(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: symbol.logical_symbol_id,
                ..Default::default()
            },
        })
        .unwrap()
        .memory;

    let path_title = "Path note";
    let path_body = "A note anchored to the file, not a symbol; its body is long enough to matter.";
    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: path_title.to_string(),
        body: path_body.to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            path: Some("src/lib.rs".to_string()),
            ..Default::default()
        },
    })
    .unwrap();

    // Seed compacted summaries + one verdict, keyed exactly like the dream passes stamp them.
    let conn = db.storage.connection();
    let repo_id: String = conn
        .query_row("SELECT repo_id FROM repo_memories WHERE id = ?1", [&sym_mem.memory_id], |r| {
            r.get(0)
        })
        .unwrap();
    let seed_summary = |id: &str, title: &str, body: &str, summary: &str| {
        conn.execute(
            "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, \
             prompt_version, generated_at_ms) VALUES (?1,?2,?3,?4,?5,0)",
            rusqlite::params![
                id,
                repo_id,
                rag_rat_query::memory::evidence::note_content_hash(title, body),
                summary,
                rag_rat_query::memory::evidence::COMPACT_PROMPT_VERSION
            ],
        )
        .unwrap();
    };
    seed_summary(&sym_mem.memory_id, sym_title, sym_body, "Keep target's invariant.");
    let path_id: String = conn
        .query_row("SELECT id FROM repo_memories WHERE title = ?1", [path_title], |r| r.get(0))
        .unwrap();
    seed_summary(&path_id, path_title, path_body, "File-level note gist.");
    let inputs = rag_rat_query::memory::evidence::checked_inputs_hash(
        conn,
        &sym_mem.memory_id,
        &Some(repo_id.clone()),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
         checked_against_commit, checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
         (?1,?2,?3,'diverged',NULL,?4,?5,0)",
        rusqlite::params![
            sym_mem.memory_id,
            repo_id,
            rag_rat_query::memory::evidence::note_content_hash(sym_title, sym_body),
            inputs,
            rag_rat_query::memory::evidence::VERDICT_PROMPT_VERSION
        ],
    )
    .unwrap();

    // memory_for_symbol: body deferred, summary + verdict present, structure kept.
    let by_symbol = db.memory_for_symbol(&symbol, 10, MemorySurface::Summary).unwrap();
    let m = by_symbol.iter().find(|m| m.memory_id == sym_mem.memory_id).expect("symbol memory");
    assert_eq!(m.body, "", "body deferred under summary");
    assert_eq!(m.summary.as_deref(), Some("Keep target's invariant."));
    assert!(
        m.verdict.as_deref().unwrap_or_default().contains("diverged"),
        "verdict: {:?}",
        m.verdict
    );
    assert!(!m.bindings.is_empty(), "the full binding structure is preserved under summary");

    // Full surface leaves the body intact and hydrates nothing.
    let full = db.memory_for_symbol(&symbol, 10, MemorySurface::Full).unwrap();
    let mf = full.iter().find(|m| m.memory_id == sym_mem.memory_id).unwrap();
    assert_eq!(mf.body, sym_body, "full surface keeps the body");
    assert_eq!(mf.summary, None);
    let chunk_id = mf.bindings.iter().find_map(|b| b.chunk_id).expect("bound chunk");

    // memory_for_path: the path-bound note defers its body too.
    let by_path = db.memory_for_path("src/lib.rs", 10, MemorySurface::Summary).unwrap();
    let mp = by_path.iter().find(|m| m.memory_id == path_id).expect("path memory");
    assert_eq!(mp.body, "");
    assert_eq!(mp.summary.as_deref(), Some("File-level note gist."));

    // read_chunk memory attachments (and the include_memories=false wrapper stays exercised).
    let chunk = db
        .read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            true,
            MemorySurface::Summary,
        )
        .unwrap()
        .expect("chunk");
    let cm =
        chunk.memories.iter().find(|m| m.memory_id == sym_mem.memory_id).expect("chunk memory");
    assert_eq!(cm.body, "");
    assert_eq!(cm.summary.as_deref(), Some("Keep target's invariant."));
    assert!(db.read_chunk_with_graph(chunk_id, GraphMetaMode::Full, 20).unwrap().is_some());

    // find_callers / trace_callees evidence.
    let evidence = db
        .memory_evidence_for_symbol_and_edges(&symbol, &[], &[], 10, MemorySurface::Summary)
        .unwrap();
    let em =
        evidence.direct.iter().find(|m| m.memory_id == sym_mem.memory_id).expect("evidence memory");
    assert_eq!(em.body, "");
    assert_eq!(em.summary.as_deref(), Some("Keep target's invariant."));

    let _ = fs::remove_dir_all(&root);
}

/// #463: a node created with NO binding target is UNANCHORED — a graph node (a `Concept` /
/// standalone `Task`) with no code anchor. It surfaces in the general `memory list` with blank
/// binding columns, dedupes against another unanchored node of the same text, is excluded by a
/// binding-kind filter, and is never flagged by `memory_validate` (no anchor to go stale/gone).
#[test]
fn unanchored_node_is_created_listed_and_deduped() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let make = || rag_rat_query::memory::RepoMemoryCreate {
        // Only Task/Concept may be created UNANCHORED (#465); other kinds must anchor to code.
        kind: "Concept".to_string(),
        title: "Prefer the event log over polling".to_string(),
        body: "A cross-cutting concept not anchored to any one symbol.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test-agent".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(), // empty → unanchored
    };

    let created = db.memory_create(make()).unwrap();
    assert!(!created.duplicate);
    assert!(created.memory.bindings.is_empty(), "an unanchored node has zero bindings");

    let conn = db.storage.connection();

    // Surfaces in the general list with blank binding columns (LEFT JOIN).
    let all = rag_rat_query::memory::list_memories(conn, None).unwrap();
    let summary = all
        .iter()
        .find(|s| s.memory_id == created.memory.memory_id)
        .expect("unanchored node must appear in `memory list`");
    assert_eq!(summary.kind, "Concept");
    assert_eq!(summary.binding_kind, "");
    assert_eq!(summary.binding_id, "");

    // A binding-kind filter excludes it (it has no binding kind).
    assert!(
        rag_rat_query::memory::list_memories(conn, Some("path")).unwrap().is_empty(),
        "an unanchored node must not surface under a binding-kind filter"
    );

    // A second unanchored node with identical text dedupes to the same id.
    let again = db.memory_create(make()).unwrap();
    assert!(again.duplicate, "a second unanchored node with the same text dedupes");
    assert_eq!(again.memory.memory_id, created.memory.memory_id);

    // Validation never flags an unanchored node (no anchor to go stale/gone).
    assert_eq!(db.memory_validate().unwrap().stale, 0);

    let _ = fs::remove_dir_all(&root);
}

/// #463: `memory_rebind` still REQUIRES an anchor — moving a memory to "no binding" is meaningless.
/// Only `memory_create` accepts the unanchored case.
#[test]
fn rebind_still_requires_a_binding_target() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: "an unanchored node".to_string(),
            body: "created without a binding".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap();

    let err = db
        .memory_rebind(
            &created.memory.memory_id,
            rag_rat_query::memory::RepoMemoryBindTarget::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("requires a binding target"),
        "rebind must reject an empty bind target: {err}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #463 guard: a PARTIALLY populated bind (an intended anchor missing a field) must ERROR, not
/// silently become an unanchored node — otherwise a typo'd anchor yields an invisible memory that
/// no `memory_for_*` lookup surfaces and validation never checks.
#[test]
fn a_partial_binding_is_rejected_not_silently_unanchored() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // tracker+project but NO item_key — an incomplete anchor, not an unanchored node.
    let err = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "partial tracker anchor".to_string(),
            body: "tracker+project without an item_key".to_string(),
            confidence: "low".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                tracker: Some("github".to_string()),
                project: Some("o/r".to_string()),
                ..Default::default()
            },
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("binding is incomplete"),
        "a partial binding must be rejected, got: {err}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #465: a polymorphic node (a `Task`/`Concept` kind) stores and round-trips an opaque JSON payload
/// through create → read → update. A non-object payload is rejected.
#[test]
fn a_polymorphic_node_stores_and_round_trips_its_payload() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A `Task` node (a new #465 kind), unanchored, carrying a structured payload.
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Task".to_string(),
            title: "Wire the payload column".to_string(),
            body: "Track the polymorphic payload work.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: Some(r#"{"estimate":"1d","priority":2}"#.to_string()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap();
    assert!(!created.duplicate);
    assert_eq!(created.memory.kind, "Task");
    assert!(created.memory.bindings.is_empty(), "a Task node is unanchored");
    assert_eq!(
        created.memory.payload_json.as_deref(),
        Some(r#"{"estimate":"1d","priority":2}"#),
        "the payload round-trips verbatim on create"
    );

    // Read back independently.
    let fetched = db.memory_get(&created.memory.memory_id).unwrap().expect("memory");
    assert_eq!(fetched.payload_json.as_deref(), Some(r#"{"estimate":"1d","priority":2}"#));

    // Update just the payload; `None` on the other fields leaves them unchanged.
    let updated = db
        .memory_update(rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: created.memory.memory_id.clone(),
            kind: None,
            title: None,
            body: None,
            confidence: None,
            status: None,
            tags: None,
            payload_json: Some(r#"{"priority":1}"#.to_string()),
        })
        .unwrap();
    assert_eq!(updated.payload_json.as_deref(), Some(r#"{"priority":1}"#));
    assert_eq!(updated.title, "Wire the payload column", "other fields unchanged");

    // A non-object payload (an array) is rejected.
    let err = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: "bad payload".to_string(),
            body: "an array is not a valid payload".to_string(),
            confidence: "low".to_string(),
            created_by: None,
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: Some("[1,2,3]".to_string()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("must be a JSON object"),
        "a non-object payload must be rejected, got: {err}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #465: dedup folds the payload — two polymorphic nodes with identical text but DIFFERENT payloads
/// are distinct (neither silently collapses onto the other, dropping its payload); identical text
/// AND payload dedups.
#[test]
fn payload_bearing_nodes_dedupe_on_payload_not_just_text() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let make = |payload: &str| rag_rat_query::memory::RepoMemoryCreate {
        kind: "Task".to_string(),
        title: "same title".to_string(),
        body: "same body".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test-agent".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: Some(payload.to_string()),
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    };

    let a = db.memory_create(make(r#"{"priority":1}"#)).unwrap();
    assert!(!a.duplicate);
    // Same text, DIFFERENT payload → a distinct node, not a duplicate.
    let b = db.memory_create(make(r#"{"priority":2}"#)).unwrap();
    assert!(!b.duplicate, "a different payload must not dedup onto the first node");
    assert_ne!(a.memory.memory_id, b.memory.memory_id);
    // Same text AND payload → a duplicate.
    let c = db.memory_create(make(r#"{"priority":1}"#)).unwrap();
    assert!(c.duplicate, "identical text and payload dedups");
    assert_eq!(c.memory.memory_id, a.memory.memory_id);

    let _ = fs::remove_dir_all(&root);
}

/// #465 (PR #471 review): dedup folds KIND, and the unanchored-create gate is kind-aware. Two
/// distinct graph-node kinds sharing text+payload are NOT duplicates; and only Task/Concept may be
/// created unanchored — an unanchored Decision is rejected (which keeps `create` in lock-step with
/// the dream verifier's kind exemption).
#[test]
fn dedup_and_unanchored_create_are_kind_aware() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let unanchored = |kind: &str| rag_rat_query::memory::RepoMemoryCreate {
        kind: kind.to_string(),
        title: "same text".to_string(),
        body: "same body".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("test-agent".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: Some(r#"{"p":1}"#.to_string()),
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    };

    // A Concept and a Task with identical text+payload are DISTINCT (dedup folds kind).
    let concept = db.memory_create(unanchored("Concept")).unwrap();
    assert!(!concept.duplicate);
    let task = db.memory_create(unanchored("Task")).unwrap();
    assert!(!task.duplicate, "a different kind is not a duplicate");
    assert_ne!(concept.memory.memory_id, task.memory.memory_id);
    // Re-creating the same kind+text+payload dedups.
    let again = db.memory_create(unanchored("Concept")).unwrap();
    assert!(again.duplicate, "identical kind+text+payload dedups");
    assert_eq!(again.memory.memory_id, concept.memory.memory_id);

    // A non-Task/Concept kind cannot be created UNANCHORED (no payload here, so the anchor gate is
    // what fires).
    let err = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "unanchored decision".to_string(),
            body: "b".to_string(),
            confidence: "low".to_string(),
            created_by: None,
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("must anchor to code"),
        "an unanchored Decision must be rejected, got: {err}"
    );
    // A payload is rejected on a non-polymorphic kind, even when ANCHORED to code.
    let perr = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "invariant with payload".to_string(),
            body: "b".to_string(),
            confidence: "low".to_string(),
            created_by: None,
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: Some(r#"{"p":1}"#.to_string()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/lib.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap_err();
    assert!(
        perr.to_string().contains("only Task/Concept may have a payload"),
        "a payload on a non-polymorphic kind must be rejected, got: {perr}"
    );

    // The invariant holds on UPDATE too: a zero-binding node cannot be retyped to a non-graph kind,
    // but retyping between graph-node kinds (Task -> Concept) is fine.
    let retype = |kind: &str| rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: task.memory.memory_id.clone(),
        kind: Some(kind.to_string()),
        title: None,
        body: None,
        confidence: None,
        status: None,
        tags: None,
        payload_json: None,
    };
    let bad = db.memory_update(retype("Decision")).unwrap_err();
    assert!(
        bad.to_string().contains("only Task/Concept may be unanchored"),
        "retyping an unanchored node to Decision must be rejected, got: {bad}"
    );
    assert_eq!(
        db.memory_update(retype("Concept")).unwrap().kind,
        "Concept",
        "retyping between graph-node kinds is allowed"
    );

    // Retyping an ANCHORED Task (carrying a payload) to a non-polymorphic kind is allowed (it has a
    // binding) and CLEARS the stranded payload rather than preserving it.
    let anchored = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Task".to_string(),
            title: "anchored task".to_string(),
            body: "b".to_string(),
            confidence: "medium".to_string(),
            created_by: None,
            source: Some("agent".to_string()),
            tags: vec![],
            payload_json: Some(r#"{"p":9}"#.to_string()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/lib.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    assert_eq!(anchored.memory.payload_json.as_deref(), Some(r#"{"p":9}"#));
    let retyped = db
        .memory_update(rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: anchored.memory.memory_id.clone(),
            kind: Some("Decision".to_string()),
            title: None,
            body: None,
            confidence: None,
            status: None,
            tags: None,
            payload_json: None,
        })
        .unwrap();
    assert_eq!(retyped.kind, "Decision");
    assert!(retyped.payload_json.is_none(), "retyping away from Task/Concept clears the payload");

    // A LEGACY zero-binding non-graph memory (a pre-gate Decision, seeded directly under the active
    // repo) stays CLEANABLE: a status-only update (mark_obsolete) does not change the kind, so the
    // gate does not trap it.
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id)
             SELECT 'mem_legacy', 'Decision', 'legacy orphan', 'b', 'low', 'active', 'agent', 0, \
             0, 'agent', 'v1', repo_id FROM repo_memories WHERE id = ?1",
            [&concept.memory.memory_id],
        )
        .unwrap();
    assert_eq!(
        db.memory_mark_obsolete("mem_legacy").unwrap().status,
        "obsolete",
        "a legacy unanchored non-graph memory can still be cleaned up"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #464: typed edges — a task `depends_on` another (forward `edges_from` + reverse `edges_into`), a
/// task `tracks` a github issue, `edge_key` is stable/idempotent, `remove` works, self-loops are
/// rejected, and an edge into an ABSENT repo is stored `unresolved` (not an error).
#[test]
fn typed_edges_add_traverse_and_resolve() {
    use rag_rat_query::memory::EdgeTarget;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let task = |title: &str| rag_rat_query::memory::RepoMemoryCreate {
        kind: "Task".to_string(),
        title: title.to_string(),
        body: "b".to_string(),
        confidence: "medium".to_string(),
        created_by: Some("t".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    };
    let a = db.memory_create(task("task A")).unwrap().memory.memory_id;
    let b = db.memory_create(task("task B")).unwrap().memory.memory_id;
    let node = |id: &str| EdgeTarget::Node { repo_id: None, node_id: id.to_string() };

    // A depends_on B (same repo).
    let edge = db.memory_edge_add(&a, "depends_on", node(&b)).unwrap();
    assert_eq!(edge.relation, "depends_on");
    assert_eq!(edge.target_node_id.as_deref(), Some(b.as_str()));
    assert_eq!(edge.anchor_status, "current");

    // Forward: edges_from(A) sees it. Reverse: edges_into(B) is the reverse traversal.
    let from = db.memory_edges_from(&a).unwrap();
    assert_eq!(from.len(), 1);
    assert_eq!(from[0].target_node_id.as_deref(), Some(b.as_str()));
    let into = db.memory_edges_into(node(&b)).unwrap();
    assert_eq!(into.len(), 1);
    assert_eq!(into[0].source_node_id, a);

    // Idempotent: re-adding the same logical edge keeps the SAME edge_key (no duplicate row).
    let again = db.memory_edge_add(&a, "depends_on", node(&b)).unwrap();
    assert_eq!(again.edge_key, edge.edge_key);
    assert_eq!(db.memory_edges_from(&a).unwrap().len(), 1);

    // A tracks a github issue — reverse-bindable "issue <- task".
    let gh = || EdgeTarget::Github { owner: "o".to_string(), repo: "r".to_string(), number: 42 };
    db.memory_edge_add(&a, "tracks", gh()).unwrap();
    let tracking = db.memory_edges_into(gh()).unwrap();
    assert_eq!(tracking.len(), 1);
    assert_eq!(tracking[0].source_node_id, a);
    assert_eq!(tracking[0].relation, "tracks");

    // A cross-repo edge into an ABSENT repo is stored `unresolved`, never a hard failure.
    let cross = db
        .memory_edge_add(&a, "relates_to", EdgeTarget::Node {
            repo_id: Some("some-other-repo".to_string()),
            node_id: "mem_absent".to_string(),
        })
        .unwrap();
    assert_eq!(cross.anchor_status, "unresolved");
    assert!(cross.target_node_id.is_none());

    // Re-resolution on READ: once the previously-absent target is indexed, a later read shows the
    // edge `current` with its target self-healed (the stored `unresolved` was only an add-time
    // snapshot). Seed the target node directly under its sibling repo (id copied from `a`, repo
    // overridden) so the id lookup finds it.
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, input_hash, memory_version, repo_id) SELECT \
             'mem_absent', kind, title, body, confidence, status, created_by, created_at_ms, \
             updated_at_ms, source, 'reresolve-hash', memory_version, 'some-other-repo' FROM \
             repo_memories WHERE id = ?1",
            [&a],
        )
        .unwrap();
    let healed = db.memory_edges_from(&a).unwrap();
    let cross_now = healed.iter().find(|e| e.edge_key == cross.edge_key).unwrap();
    assert_eq!(
        cross_now.anchor_status, "current",
        "an unresolved edge re-resolves once its target is indexed"
    );
    assert_eq!(cross_now.target_node_id.as_deref(), Some("mem_absent"));
    assert_eq!(
        cross_now.target_repo_id, "some-other-repo",
        "target_repo_id self-heals from the node"
    );

    // An IMPLICIT cross-repo target (no explicit repo_id) that resolves to a SIBLING repo is
    // rejected — `mem_absent` now lives in `some-other-repo`, so a bare `node()` edge to it must be
    // made explicit. (The `relates_to` edge above was allowed only because it named the repo.)
    let implicit = db.memory_edge_add(&a, "depends_on", node("mem_absent")).unwrap_err();
    assert!(implicit.to_string().contains("is not a node in this repo"), "{implicit}");

    // EXPLICIT cross-repo whose id resolves to a DIFFERENT repo than named → rejected (`mem_absent`
    // lives in `some-other-repo`, not the named `wrong-repo`).
    let mismatch = db
        .memory_edge_add(&a, "depends_on", rag_rat_query::memory::EdgeTarget::Node {
            repo_id: Some("wrong-repo".to_string()),
            node_id: "mem_absent".to_string(),
        })
        .unwrap_err();
    assert!(mismatch.to_string().contains("not the named `wrong-repo`"), "{mismatch}");

    // EXPLICIT cross-repo into a REGISTERED repo but the node is absent → a typo, rejected. (An
    // UNREGISTERED repo is instead a legitimate deferred `unresolved` reference — the `relates_to`
    // edge above.)
    db.storage
        .connection()
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
             ('some-other-repo', 'other', 0)",
            [],
        )
        .unwrap();
    let typo = db
        .memory_edge_add(&a, "depends_on", rag_rat_query::memory::EdgeTarget::Node {
            repo_id: Some("some-other-repo".to_string()),
            node_id: "mem_ghost".to_string(),
        })
        .unwrap_err();
    assert!(typo.to_string().contains("is not a node in repo `some-other-repo`"), "{typo}");

    // A self-loop is rejected.
    let err = db.memory_edge_add(&a, "relates_to", node(&a)).unwrap_err();
    assert!(err.to_string().contains("cannot point a node at itself"), "{err}");

    // A SAME-repo target that doesn't exist is a typo → rejected (a cross-repo absent target is
    // fine, as the `relates_to` edge above stored `unresolved`).
    let missing = db.memory_edge_add(&a, "depends_on", node("mem_nonexistent")).unwrap_err();
    assert!(missing.to_string().contains("is not a node in this repo"), "{missing}");

    // Remove by edge_key.
    assert!(db.memory_edge_remove(&edge.edge_key).unwrap());
    assert!(db.memory_edges_from(&a).unwrap().iter().all(|e| e.edge_key != edge.edge_key));

    // An obsoleted SOURCE node's edges drop out of both traversals — a hidden node's relationships
    // are dead. `a` still owns the `tracks` and (now-resolved) `relates_to` edges here.
    assert!(
        !db.memory_edges_from(&a).unwrap().is_empty(),
        "sanity: a has live edges before obsolete"
    );
    db.memory_mark_obsolete(&a).unwrap();
    assert!(
        db.memory_edges_from(&a).unwrap().is_empty(),
        "obsolete source has no live outgoing edges"
    );
    assert!(
        db.memory_edges_into(gh()).unwrap().is_empty(),
        "reverse traversal drops an obsolete source's edges"
    );
    // ...and you cannot author a NEW edge FROM an obsolete source (the add-time twin of the
    // filter).
    let from_dead = db.memory_edge_add(&a, "depends_on", node(&b)).unwrap_err();
    assert!(from_dead.to_string().contains("not found or is obsolete"), "{from_dead}");

    let _ = fs::remove_dir_all(&root);
}

/// #492: a single `gone` observation must not persist a downgrade — a validate pass racing a
/// rebuild window (or sweeping from a narrower checkout context) can produce a torn observation,
/// and doctor then hands out destructive mark-obsolete advice for healthy anchors. The persisted
/// `anchor_status` downgrades only on the SECOND consecutive gone observation; the validation
/// report still counts what each pass actually saw.
#[test]
fn a_downgrade_to_gone_needs_two_consecutive_observations() {
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
            title: "Anchored to a file a torn pass will misjudge".to_string(),
            body: "One gone observation arms the marker; only the second downgrades.".to_string(),
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
    let persisted = |db: &IndexDatabase| -> (String, Option<i64>) {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status, downgrade_pending_at_ms FROM repo_memory_bindings
                 WHERE memory_id = ?1",
                params![memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    db.memory_validate().unwrap();
    assert_eq!(persisted(&db), ("current".to_string(), None), "alive file → current, unarmed");

    fs::remove_file(root.join("src/doomed.rs")).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'src/doomed.rs'",
            [],
        )
        .unwrap();

    // First gone observation: the report says what the pass saw, but the persisted status holds
    // and the marker arms.
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1, "the report counts the computed observation");
    let (status, marker) = persisted(&db);
    assert_eq!(status, "current", "one observation must not persist the downgrade");
    assert!(marker.is_some(), "the first gone observation arms the marker");

    // Second consecutive observation: the downgrade lands and the marker clears.
    let report = db.memory_validate().unwrap();
    assert_eq!(report.gone, 1);
    assert_eq!(
        persisted(&db),
        ("gone".to_string(), None),
        "the second consecutive observation persists the downgrade and clears the marker"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #492: a positive observation between two gone observations disarms the pending downgrade — the
/// ping-pong a pair of checkout contexts produces (one sees the anchor, one does not) must never
/// land a persisted `gone`.
#[test]
fn a_recovered_anchor_clears_the_pending_downgrade() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keeper() {}\n").unwrap();
    fs::write(root.join("src/wobbling.rs"), "pub fn wobbling() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchored to a file that wobbles across contexts".to_string(),
            body: "A recovery between gone observations must disarm the downgrade.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/wobbling.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id;
    let persisted = |db: &IndexDatabase| -> (String, Option<i64>) {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status, downgrade_pending_at_ms FROM repo_memory_bindings
                 WHERE memory_id = ?1",
                params![memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };

    fs::remove_file(root.join("src/wobbling.rs")).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'src/wobbling.rs'",
            [],
        )
        .unwrap();
    db.memory_validate().unwrap();
    let (status, marker) = persisted(&db);
    assert_eq!(status, "current");
    assert!(marker.is_some(), "the gone observation arms the marker");

    // The anchor recovers (the other context's view): the next pass re-asserts it and disarms
    // the half-armed downgrade.
    fs::write(root.join("src/wobbling.rs"), "pub fn wobbling() {}\n").unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'source', sha256 = 'restored'
              WHERE path = 'src/wobbling.rs'",
            [],
        )
        .unwrap();
    db.memory_validate().unwrap();
    assert_eq!(
        persisted(&db),
        ("current".to_string(), None),
        "a positive observation stamps current and clears the marker"
    );

    // A later gone observation starts the two-pass rule from scratch.
    fs::remove_file(root.join("src/wobbling.rs")).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'src/wobbling.rs'",
            [],
        )
        .unwrap();
    db.memory_validate().unwrap();
    let (status, marker) = persisted(&db);
    assert_eq!(status, "current", "the disarmed marker means the count restarts at one");
    assert!(marker.is_some());

    let _ = fs::remove_dir_all(&root);
}

/// #492: while a STAGED generation exists for the repo (a rebuild is mid-flight, or an abandoned
/// staging awaits gc), a gone observation is untrustworthy — the pass may be reading a
/// half-published world. It must neither ARM nor CONFIRM a downgrade; the two-pass rule resumes
/// once the staging clears.
#[test]
fn a_staged_generation_freezes_the_downgrade_rule() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keeper() {}\n").unwrap();
    fs::write(root.join("src/torn.rs"), "pub fn torn() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "Anchored while a rebuild is mid-flight".to_string(),
            body: "A torn window's gone observation must not move the downgrade rule.".to_string(),
            confidence: "medium".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/torn.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id;
    let persisted = |db: &IndexDatabase| -> (String, Option<i64>) {
        db.storage
            .connection()
            .query_row(
                "SELECT anchor_status, downgrade_pending_at_ms FROM repo_memory_bindings
                 WHERE memory_id = ?1",
                params![memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };

    fs::remove_file(root.join("src/torn.rs")).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'src/torn.rs'",
            [],
        )
        .unwrap();
    // A staged (higher-than-live) generation row: the mid-flight rebuild window.
    let live_generation =
        rag_rat_db::schema::live_files_generation(db.storage.connection(), &db.active_repo_id)
            .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files (path, language, kind, sha256, modified_at_ms, generated,
                                indexed_at_ms, indexed_revision, commit_sha, worktree_id,
                                has_test_code, repo_id, generation)
             VALUES ('src/staged.rs', 'rust', 'source', 'staged', 0, 0, 0, '', '', '', 0, ?1, ?2)",
            params![db.active_repo_id, live_generation + 1],
        )
        .unwrap();

    // Repeated gone observations inside the torn window: nothing arms, nothing confirms.
    db.memory_validate().unwrap();
    db.memory_validate().unwrap();
    assert_eq!(
        persisted(&db),
        ("current".to_string(), None),
        "a torn window's observations neither arm nor confirm the downgrade"
    );

    // The staging clears (published or gc-swept): the two-pass rule proceeds.
    db.storage
        .connection()
        .execute("DELETE FROM main.files WHERE path = 'src/staged.rs'", [])
        .unwrap();
    db.memory_validate().unwrap();
    let (status, marker) = persisted(&db);
    assert_eq!(status, "current");
    assert!(marker.is_some(), "the first trustworthy gone observation arms the marker");
    db.memory_validate().unwrap();
    assert_eq!(persisted(&db), ("gone".to_string(), None), "the second confirms");

    let _ = fs::remove_dir_all(&root);
}

/// #493: a key-derivation change (grammar bump, signature-capture/kind/qualified-name fix) churns
/// EVERY logical id at the next rebuild — the stored rows hold old-derivation field values, so
/// `realign_logical_symbol_ids` (which recomputes from stored fields) reproduces the old ids and
/// cannot help. On a `logical_key_version` mismatch the rebuild snapshots the REFERENCED old rows
/// before the clear and realigns them onto the re-derived ids by (path, name, kind) + qualified-
/// name/signature agreement — whole-repo stranding becomes an invisible in-place migration.
#[test]
fn key_derivation_drift_realigns_referenced_logical_bindings_at_rebuild() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn drift_anchor() -> u32 { 7 }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let real_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'drift_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Anchored across derivation drift".to_string(),
            body: "The binding must follow the symbol through a logical-key version bump."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(real_id),
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
    drop(db);

    // Simulate an old-derivation store: the persisted logical id (and every reference to it) was
    // derived under PREVIOUS key rules — the re-derive will mint a different id. A raw connection
    // (foreign keys off) rewrites parent + references consistently, and clearing the stamp is
    // what arms the heal.
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = 424242 WHERE id = ?1", params![real_id])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    // A content change so the next rebuild runs a full pass (unchanged content short-circuits).
    fs::write(
        root.join("src/lib.rs"),
        "pub fn drift_anchor() -> u32 { 7 }\n\npub fn drift_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let fresh_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'drift_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bound, fresh_id,
        "the drift heal must realign the binding onto the re-derived logical id"
    );
    let stamp: String = db
        .storage
        .connection()
        .query_row("SELECT value FROM repo_meta WHERE key = 'logical_key_version'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(!stamp.is_empty(), "the rebuild stamps the healed logical-key version");

    // End-to-end: validate resolves the realigned binding as current, not gone/relocated.
    db.memory_validate().unwrap();
    let status: String = db
        .storage
        .connection()
        .query_row(
            "SELECT anchor_status FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "current");

    let _ = fs::remove_dir_all(&root);
}

/// #493 version gate: a CURRENT `logical_key_version` stamp means the derivation did not change,
/// so the rebuild must not run the drift snapshot/heal — a dangling reference then stays dangling
/// for the validate-time relocation ladder (the pre-#493 behavior).
#[test]
fn a_current_logical_key_version_stamp_skips_the_drift_heal() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn gated_anchor() -> u32 { 7 }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let real_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'gated_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Gated by the version stamp".to_string(),
            body: "No drift heal may run while the stamp is current.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(real_id),
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
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = 424242 WHERE id = ?1", params![real_id])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![real_id],
        )
        .unwrap();
        // The stamp stays CURRENT (written by the first rebuild) — the heal must not arm.
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn gated_anchor() -> u32 { 7 }\n\npub fn gated_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bound, 424242,
        "with a current stamp the heal must not run; the ladder owns this binding"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #493 ambiguity posture: overload twins (same path/name/kind, same qualified name, distinct
/// signatures — cfg variants whose signatures differ) whose signature evidence ALSO drifted
/// cannot be told apart; the heal must match NEITHER (a confidently mis-anchored memory is worse
/// than a flagged one) and leave both to the relocation ladder.
#[test]
fn drifted_overload_twins_stay_unmatched_for_the_ladder() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn drift_twin(a: u32) -> u32 { a }\n#[cfg(windows)]\npub fn \
         drift_twin(b: i64) -> i64 { b }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let twin_ids: Vec<i64> = {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare("SELECT id FROM logical_symbols WHERE logical_name = 'drift_twin' ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(twin_ids.len(), 2, "the fixture must produce two signature-distinct twins");

    let mut memory_ids = Vec::new();
    for (i, twin) in twin_ids.iter().enumerate() {
        let created = db
            .memory_create(rag_rat_query::memory::RepoMemoryCreate {
                kind: "Invariant".to_string(),
                title: format!("Bound to drift twin {i}"),
                body: "Ambiguous drift must not guess a twin.".to_string(),
                confidence: "high".to_string(),
                created_by: Some("test-agent".to_string()),
                source: Some("agent".to_string()),
                tags: Vec::new(),
                payload_json: None,
                bind: rag_rat_query::memory::RepoMemoryBindTarget {
                    logical_symbol_id: Some(*twin),
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
        memory_ids.push(created.memory.memory_id);
    }
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        for (i, twin) in twin_ids.iter().enumerate() {
            let fake = 424242 + i as i64;
            conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![fake, twin])
                .unwrap();
            conn.execute(
                "UPDATE logical_symbol_members SET logical_symbol_id = ?1
                  WHERE logical_symbol_id = ?2",
                params![fake, twin],
            )
            .unwrap();
            conn.execute(
                "UPDATE repo_memory_bindings SET logical_symbol_id = ?1
                  WHERE logical_symbol_id = ?2",
                params![fake, twin],
            )
            .unwrap();
        }
        // Signature capture ALSO drifted: the stored member signatures no longer agree with the
        // re-derived ones, so signature evidence cannot break the qualified-name tie.
        conn.execute(
            "UPDATE symbols SET signature = signature || ' /* drifted */'
              WHERE id IN (SELECT symbol_id FROM logical_symbol_members
                            WHERE logical_symbol_id IN (424242, 424243))",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn drift_twin(a: u32) -> u32 { a }\n#[cfg(windows)]\npub fn \
         drift_twin(b: i64) -> i64 { b }\n\npub fn twin_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    for (i, memory_id) in memory_ids.iter().enumerate() {
        let bound: i64 = db
            .storage
            .connection()
            .query_row(
                "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
                params![memory_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            bound,
            424242 + i as i64,
            "ambiguous twins must stay unmatched (memory {i}); the ladder owns them"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

/// #493: when the QUALIFIED-NAME derivation drifted (the stored qualified name no longer matches
/// the re-derived one), signature agreement is the evidence that realigns the binding.
#[test]
fn qualified_name_drift_realigns_via_signature_agreement() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn qual_drift_anchor(x: u8) -> u8 { x }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let real_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT id FROM logical_symbols WHERE logical_name = 'qual_drift_anchor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Anchored across qualified-name drift".to_string(),
            body: "Signature agreement must carry the realign when the qual drifted.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(real_id),
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
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // The stored row carries the OLD-derivation qualified name; the signature is unchanged.
        conn.execute(
            "INSERT OR IGNORE INTO name_strings(value) VALUES ('legacy::qual_drift_anchor')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbols
                SET id = 424242,
                    qualified_name_id =
                        (SELECT id FROM name_strings WHERE value = 'legacy::qual_drift_anchor')
              WHERE id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn qual_drift_anchor(x: u8) -> u8 { x }\n\npub fn qual_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let fresh_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT id FROM logical_symbols WHERE logical_name = 'qual_drift_anchor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bound, fresh_id, "signature agreement must realign a qual-drifted binding");

    let _ = fs::remove_dir_all(&root);
}

/// #493: when the strict pass yields MULTIPLE evidence-eligible candidates (same-qualified-name
/// twins differing only by signature — cfg overloads with distinct signatures group into
/// separate logical symbols), the winner is the UNIQUE one agreeing on BOTH axes. `old` shares
/// its qualified name with both twins, so both are eligible; only the twin whose signature also
/// matches is the both-axes winner, and the realign lands there rather than falling to the
/// ladder. Exercises `unique_drift_winner`'s narrowing arm, not the single-candidate path.
#[test]
fn multiple_eligible_candidates_narrow_to_the_both_axes_winner() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two cfg overloads with DIFFERENT signatures → two logical symbols sharing one qualified
    // name. tree-sitter indexes both cfg branches, so both defs are present.
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn over_pick(a: u32) -> u32 { a }\n#[cfg(windows)]\npub fn \
         over_pick(a: u64) -> u64 { a }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    // The u32 twin — the one whose signature the drifted snapshot will carry.
    let u32_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT ls.id FROM logical_symbols ls
             JOIN logical_symbol_members m ON m.logical_symbol_id = ls.id
             JOIN symbols s ON s.id = m.symbol_id
             WHERE ls.logical_name = 'over_pick' AND s.signature LIKE '%u32%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let u64_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT ls.id FROM logical_symbols ls
             JOIN logical_symbol_members m ON m.logical_symbol_id = ls.id
             JOIN symbols s ON s.id = m.symbol_id
             WHERE ls.logical_name = 'over_pick' AND s.signature LIKE '%u64%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(u32_id, u64_id, "the differing signatures must produce two logical symbols");
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Anchored to the u32 overload across drift".to_string(),
            body: "Both twins agree on qual; only the u32 twin agrees on both axes.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(u32_id),
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
    drop(db);

    // Drift only the u32 twin's id to a fake (qual + signature stored unchanged): after the
    // rebuild both twins re-derive fresh, both share `old`'s qualified name, and the u32 twin is
    // the unique both-axes agreer.
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = 424242 WHERE id = ?1", params![u32_id])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![u32_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![u32_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn over_pick(a: u32) -> u32 { a }\n#[cfg(windows)]\npub fn \
         over_pick(a: u64) -> u64 { a }\n\npub fn pick_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let fresh_u32_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT ls.id FROM logical_symbols ls
             JOIN logical_symbol_members m ON m.logical_symbol_id = ls.id
             JOIN symbols s ON s.id = m.symbol_id
             WHERE ls.logical_name = 'over_pick' AND s.signature LIKE '%u32%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bound, fresh_u32_id,
        "the both-axes winner (the signature-matching twin) inherits the drifted binding"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a KIND-classification drift (an advertised bump reason) leaves the stored row
/// with the old kind, so the strict (path, name, kind) candidate pass finds nothing — the heal
/// must retry kind-relaxed and let qualified-name + signature agreement carry the realign,
/// instead of stamping the repo healed with the reference still stranded.
#[test]
fn kind_classification_drift_realigns_via_the_relaxed_pass() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn kind_drift_anchor(k: u8) -> u8 { k }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let real_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT id FROM logical_symbols WHERE logical_name = 'kind_drift_anchor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Anchored across kind drift".to_string(),
            body: "Qualified-name and signature agreement must carry a kind reclassification."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(real_id),
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
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // The stored row carries the OLD kind classification; qualified name and signature are
        // unchanged.
        conn.execute(
            "UPDATE logical_symbols SET id = 424242, kind = 'legacy_kind' WHERE id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![real_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn kind_drift_anchor(k: u8) -> u8 { k }\n\npub fn kind_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let fresh_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT id FROM logical_symbols WHERE logical_name = 'kind_drift_anchor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bound, fresh_id, "the kind-relaxed pass must realign a kind-drifted binding");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a surviving OLD id is not proof of an unchanged symbol — after a key change a
/// DIFFERENT symbol's re-derived key can occupy it (a key swap, not a hash collision). The heal
/// must compare the survivor's key fields against the snapshot, realign the drifted reference by
/// evidence, and leave the occupying row (and its members) untouched — a reference-only remap.
#[test]
fn an_old_id_occupied_by_another_symbol_still_realigns_by_evidence() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha_occ(a: u8) -> u8 { a }\n\npub fn beta_occ(b: u16) -> u16 { b }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let id_of = |db: &IndexDatabase, name: &str| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT id FROM logical_symbols WHERE logical_name = ?1", [name], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let alpha_id = id_of(&db, "alpha_occ");
    let beta_id = id_of(&db, "beta_occ");
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Anchored to alpha across an id swap".to_string(),
            body: "A surviving id occupied by beta must not capture alpha's binding.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(alpha_id),
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
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Simulate the swap: under OLD derivation rules alpha's key hashed to what is beta's id
        // under the NEW rules. Move beta's stored row aside and park alpha's row (and its
        // references) on beta's id.
        conn.execute(
            "DELETE FROM logical_symbol_members
              WHERE logical_symbol_id = ?1",
            params![beta_id],
        )
        .unwrap();
        conn.execute("DELETE FROM logical_symbols WHERE id = ?1", params![beta_id]).unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![
            beta_id, alpha_id
        ])
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha_occ(a: u8) -> u8 { a }\n\npub fn beta_occ(b: u16) -> u16 { b }\n\npub fn \
         occ_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let alpha_fresh = id_of(&db, "alpha_occ");
    let beta_fresh = id_of(&db, "beta_occ");
    assert_eq!(beta_fresh, beta_id, "beta re-derives its own id; the fixture swap is honest");
    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bound, alpha_fresh,
        "the occupied old id must realign to alpha by evidence, not stay captured by beta"
    );
    // The occupying row is untouched by the reference-only remap: beta keeps its id and members.
    let beta_members: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM logical_symbol_members WHERE logical_symbol_id = ?1",
            params![beta_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(beta_members > 0, "beta's rebuilt members must survive the drift remap");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: when a key change MERGES two old logical symbols into one surviving row (e.g. a
/// signature normalization collapses sig-distinct cfg twins into one group under the survivor's
/// id), the vanished twin's evidence match onto the survivor competes with the survivor's own
/// existing references. The heal must not silently move it — survivors count as target claims,
/// the pair drops, and the validate-time ladder relocates it with a visible papertrail.
#[test]
fn a_merge_into_a_surviving_id_is_left_for_the_ladder_not_silently_healed() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two cfg variants with DISTINCT signatures — two logical rows sharing (path, name, qual).
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn merge_twin(a: u32) -> u32 { a }\n#[cfg(windows)]\npub fn \
         merge_twin(b: i64) -> i64 { b }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let twin_ids: Vec<i64> = {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare("SELECT id FROM logical_symbols WHERE logical_name = 'merge_twin' ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(twin_ids.len(), 2, "the fixture must produce two signature-distinct twins");

    let mut memory_ids = Vec::new();
    for (i, twin) in twin_ids.iter().enumerate() {
        let created = db
            .memory_create(rag_rat_query::memory::RepoMemoryCreate {
                kind: "Invariant".to_string(),
                title: format!("Bound to merge twin {i}"),
                body: "A merge must not silently capture the vanished twin's reference."
                    .to_string(),
                confidence: "high".to_string(),
                created_by: Some("test-agent".to_string()),
                source: Some("agent".to_string()),
                tags: Vec::new(),
                payload_json: None,
                bind: rag_rat_query::memory::RepoMemoryBindTarget {
                    logical_symbol_id: Some(*twin),
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
        memory_ids.push(created.memory.memory_id);
    }
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    // The "derivation change": both variants now carry ONE signature (byte-identical
    // declarations — the canonical cfg-variant grouping), so the rebuild collapses the group
    // under the surviving twin's key/id and the other id vanishes.
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn merge_twin(a: u32) -> u32 { a }\n#[cfg(windows)]\npub fn \
         merge_twin(a: u32) -> u32 { a }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let merged_ids: Vec<i64> = {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare("SELECT id FROM logical_symbols WHERE logical_name = 'merge_twin' ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(merged_ids.len(), 1, "the derivation change must merge the twins into one row");
    let survivor = merged_ids[0];
    let vanished_memory = memory_ids
        .iter()
        .zip(&twin_ids)
        .find(|(_, twin)| **twin != survivor)
        .map(|(memory_id, _)| memory_id.clone())
        .expect("one twin id must vanish in the merge");
    let vanished_old_id =
        *twin_ids.iter().find(|twin| **twin != survivor).expect("the vanished twin id");

    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![vanished_memory],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bound, vanished_old_id,
        "the heal must not silently move a reference onto a claimed survivor id"
    );

    // The relocation ladder owns it — with a visible papertrail, not a silent heal.
    db.memory_validate().unwrap();
    let (relocated_to, status): (i64, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id, anchor_status FROM repo_memory_bindings
             WHERE memory_id = ?1",
            params![vanished_memory],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "relocated", "the ladder relocates the merged-away reference visibly");
    assert_eq!(relocated_to, survivor);

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a logical group's snapshot signature is EVIDENCE only when every member agrees —
/// a `LIMIT 1` pick would hand a SPLIT group's references to whichever overload the arbitrary
/// member happened to match (members can disagree when a scope's symbols were re-captured while
/// a sibling scope's were not). Disagreement must yield no signature evidence at all.
#[test]
fn disagreeing_member_signatures_yield_no_drift_evidence() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn split_twin(a: u32) -> u32 { a }\n#[cfg(windows)]\npub fn \
         split_twin(a: u32) -> u32 { a }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let group_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'split_twin'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Bound to the collapsed group".to_string(),
            body: "A split group's members disagree; the snapshot must carry no signature."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(group_id),
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

    // Simulate the partially-re-captured store: the group's two members now carry DIFFERENT
    // signature captures. Arm the drift snapshot and read it directly.
    {
        let conn = db.storage.connection();
        let member_symbols: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT symbol_id FROM logical_symbol_members WHERE logical_symbol_id = ?1
                     ORDER BY symbol_id",
                )
                .unwrap();
            stmt.query_map(params![group_id], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        assert_eq!(member_symbols.len(), 2, "the cfg twins must group into one two-member row");
        conn.execute(
            "UPDATE symbols SET signature = 'fn split_twin(recaptured: u32) -> u32'
              WHERE id = ?1",
            params![member_symbols[0]],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    let snapshot =
        db.logical_key_drift_snapshot().unwrap().expect("a missing stamp arms the drift snapshot");
    let row = snapshot
        .iter()
        .find(|row| row.old_id == group_id)
        .expect("the referenced group is snapshotted");
    assert!(
        row.signature.is_none(),
        "disagreeing member signatures must yield NO signature evidence, not an arbitrary pick"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a group with one signature-less member and one captured member is DISAGREEMENT,
/// not unanimity — `COUNT(DISTINCT)` alone ignores the NULLs and would crown the sole non-null
/// capture as the group's evidence, silently realigning references by a signature only PART of
/// the group carries. Mixed NULL/non-null must yield no signature evidence at all.
#[test]
fn a_mixed_null_signature_group_yields_no_drift_evidence() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn split_twin(a: u32) -> u32 { a }\n#[cfg(windows)]\npub fn \
         split_twin(a: u32) -> u32 { a }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let group_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'split_twin'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Bound to the partially captured group".to_string(),
            body: "One member lost its signature capture; the group must carry no evidence."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(group_id),
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

    // Simulate a partial re-capture: one member keeps its signature, the other loses it
    // entirely. Arm the drift snapshot and read it directly.
    {
        let conn = db.storage.connection();
        let member_symbols: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT symbol_id FROM logical_symbol_members WHERE logical_symbol_id = ?1
                     ORDER BY symbol_id",
                )
                .unwrap();
            stmt.query_map(params![group_id], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        assert_eq!(member_symbols.len(), 2, "the cfg twins must group into one two-member row");
        conn.execute("UPDATE symbols SET signature = NULL WHERE id = ?1", params![
            member_symbols[0]
        ])
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    let snapshot =
        db.logical_key_drift_snapshot().unwrap().expect("a missing stamp arms the drift snapshot");
    let row = snapshot
        .iter()
        .find(|row| row.old_id == group_id)
        .expect("the referenced group is snapshotted");
    assert!(
        row.signature.is_none(),
        "a mixed NULL/non-null signature group must yield NO signature evidence — COUNT(DISTINCT) \
         alone ignores the NULL member and crowns the sole capture"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: an occupied old id with NO evidence winner must be VACATED, not left in place —
/// the occupying row is live, so `memory_validate` would resolve the binding as healthy and the
/// relocation ladder would never run: a silent permanent mis-anchor. Vacated references point at
/// a sentinel that resolves to nothing, so the ladder relocates them by qualified name with a
/// visible papertrail.
#[test]
fn an_occupied_old_id_with_no_winner_is_vacated_for_the_ladder() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn vac_alpha(a: u8) -> u8 { a }\n\npub fn vac_beta(b: u16) -> u16 { b }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let id_of = |db: &IndexDatabase, name: &str| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT id FROM logical_symbols WHERE logical_name = ?1", [name], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let alpha_id = id_of(&db, "vac_alpha");
    let beta_id = id_of(&db, "vac_beta");
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Anchored to alpha across an evidence-dead swap".to_string(),
            body: "An occupied id without evidence must be vacated, never silently captured."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(alpha_id),
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
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Park alpha's row on beta's id (the swap), and kill BOTH evidence axes: the stored
        // qualified name AND the stored member signature no longer match anything re-derived.
        conn.execute(
            "DELETE FROM logical_symbol_members
              WHERE logical_symbol_id = ?1",
            params![beta_id],
        )
        .unwrap();
        conn.execute("DELETE FROM logical_symbols WHERE id = ?1", params![beta_id]).unwrap();
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('legacy::vac_alpha')", [])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbols
                SET id = ?1,
                    qualified_name_id =
                        (SELECT id FROM name_strings WHERE value = 'legacy::vac_alpha')
              WHERE id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE symbols SET signature = 'fn vac_alpha(legacy_capture: u8) -> u8'
              WHERE id IN (SELECT symbol_id FROM logical_symbol_members
                            WHERE logical_symbol_id = ?1)",
            params![beta_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn vac_alpha(a: u8) -> u8 { a }\n\npub fn vac_beta(b: u16) -> u16 { b }\n\npub fn \
         vac_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(
        bound, beta_id,
        "an occupied id without an evidence winner must be vacated, not silently captured by the \
         occupying symbol"
    );

    // The ladder relocates the vacated reference by its stored qualified name, visibly.
    db.memory_validate().unwrap();
    let (relocated_to, status): (i64, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id, anchor_status FROM repo_memory_bindings
             WHERE memory_id = ?1",
            params![memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "relocated", "the ladder owns the vacated reference, with a papertrail");
    assert_eq!(relocated_to, id_of(&db, "vac_alpha"));

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a vacated CALL-PATH endpoint must be NULLED, not parked on
/// `VACATED_LOGICAL_SYMBOL_ID`. `validate_call_path_binding` re-checks only the stored edge
/// fingerprints — it never consults or repairs `start/end_logical_symbol_id` — so a sentinel
/// there would leave a permanent bogus `sym_8000…` endpoint that hydration surfaces and no
/// validator ever fixes. NULL is the supported "no recorded endpoint" state.
#[test]
fn a_vacated_call_path_endpoint_is_nulled_not_sentineled() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn cp_alpha(a: u8) -> u8 { a }\n\npub fn cp_beta(b: u16) -> u16 { b }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let id_of = |db: &IndexDatabase, name: &str| -> i64 {
        db.storage
            .connection()
            .query_row("SELECT id FROM logical_symbols WHERE logical_name = ?1", [name], |r| {
                r.get(0)
            })
            .unwrap()
    };
    let alpha_id = id_of(&db, "cp_alpha");
    let beta_id = id_of(&db, "cp_beta");
    // A parent memory (path-bound so it needs no logical symbol) plus a call-path row whose START
    // endpoint references cp_alpha — the durable reference the drift snapshot will pick up.
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "Call-path memory with a drifting endpoint".to_string(),
            body: "Its start endpoint must vacate to NULL, not a bogus sentinel.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/lib.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id;
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memory_call_paths(memory_id, start_logical_symbol_id,
                        end_logical_symbol_id, edge_sequence_hash, path_summary, created_at_ms)
             VALUES (?1, ?2, NULL, 'cp-hash-1', 'alpha -> ...', 0)",
            params![memory_id, alpha_id],
        )
        .unwrap();
    // The matching `call_path` BINDING row also carries the endpoint in its `logical_symbol_id`
    // (start-or-end at bind time). `validate_call_path_binding` ignores it too, so the vacate must
    // NULL it — not the i64::MIN sentinel the generic binding update would leave (#493 review).
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id,
                        logical_symbol_id, anchor_status, created_at_ms)
             VALUES (?1, 'call_path', 'cp-hash-1', ?2, 'current', 0)",
            params![memory_id, alpha_id],
        )
        .unwrap();
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Occupied-no-winner swap: cp_beta re-derives to its own (unchanged) id, so parking
        // cp_alpha's row on beta_id and killing both evidence axes makes beta_id occupied by a
        // key-mismatched survivor after the rebuild, with no candidate the endpoint can realign
        // onto.
        conn.execute("DELETE FROM logical_symbol_members WHERE logical_symbol_id = ?1", params![
            beta_id
        ])
        .unwrap();
        conn.execute("DELETE FROM logical_symbols WHERE id = ?1", params![beta_id]).unwrap();
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('legacy::cp_alpha')", [])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbols
                SET id = ?1,
                    qualified_name_id =
                        (SELECT id FROM name_strings WHERE value = 'legacy::cp_alpha')
              WHERE id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE symbols SET signature = 'fn cp_alpha(legacy_capture: u8) -> u8'
              WHERE id IN (SELECT symbol_id FROM logical_symbol_members
                            WHERE logical_symbol_id = ?1)",
            params![beta_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_call_paths SET start_logical_symbol_id = ?1
              WHERE start_logical_symbol_id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2 AND binding_kind = 'call_path'",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn cp_alpha(a: u8) -> u8 { a }\n\npub fn cp_beta(b: u16) -> u16 { b }\n\npub fn \
         cp_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let start_endpoint: Option<i64> = db
        .storage
        .connection()
        .query_row(
            "SELECT start_logical_symbol_id FROM repo_memory_call_paths WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        start_endpoint, None,
        "a vacated call-path endpoint must be NULL, never the i64::MIN sentinel that no validator \
         handles"
    );
    let binding_endpoint: Option<i64> = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings
              WHERE memory_id = ?1 AND binding_kind = 'call_path'",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        binding_endpoint, None,
        "the call_path BINDING row's endpoint must also be NULL, not the sentinel the generic \
         binding update would leave"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a drifted call-path id that VANISHES (re-derives nowhere and finds no evidence
/// winner) never enters the occupied set, so the vacate loop used to skip it entirely — leaving
/// the endpoints and the call_path binding pointing at a dead pre-rebuild id. `validate_call_path`
/// never revisits those ids, and once a full rebuild stamps `logical_key_version` the heal won't
/// run again, so the stale `sym_…` would be permanent. Vanished no-winner call-path references
/// must be NULLed too, not only occupied ones.
#[test]
fn a_vanished_call_path_reference_with_no_winner_is_nulled() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn cpv_fn(a: u8) -> u8 { a }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let real_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'cpv_fn'", [], |r| r.get(0))
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: "Call-path memory whose endpoint vanishes".to_string(),
            body: "A vanished no-winner endpoint must be NULLed, not left on a dead id."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                path: Some("src/lib.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id;
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memory_call_paths(memory_id, start_logical_symbol_id,
                        end_logical_symbol_id, edge_sequence_hash, path_summary, created_at_ms)
             VALUES (?1, ?2, NULL, 'cpv-hash', 'cpv -> ...', 0)",
            params![memory_id, real_id],
        )
        .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id,
                        logical_symbol_id, anchor_status, created_at_ms)
             VALUES (?1, 'call_path', 'cpv-hash', ?2, 'current', 0)",
            params![memory_id, real_id],
        )
        .unwrap();
    drop(db);

    let fake_id: i64 = 424249;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Drift the symbol onto a fake id AND kill both evidence axes (legacy qual + legacy
        // signature), so after the rebuild the fake id VANISHES (cpv_fn re-derives to a different
        // id) and the re-derived cpv_fn is not an evidence match — a vanished no-winner id, NOT an
        // occupied one.
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('legacy::cpv_fn')", [])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbols
                SET id = ?1,
                    qualified_name_id =
                        (SELECT id FROM name_strings WHERE value = 'legacy::cpv_fn')
              WHERE id = ?2",
            params![fake_id, real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
            params![fake_id, real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE symbols SET signature = 'fn cpv_fn(legacy_capture: u8) -> u8'
              WHERE id IN (SELECT symbol_id FROM logical_symbol_members
                            WHERE logical_symbol_id = ?1)",
            params![fake_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_call_paths SET start_logical_symbol_id = ?1
              WHERE start_logical_symbol_id = ?2",
            params![fake_id, real_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2 AND binding_kind = 'call_path'",
            params![fake_id, real_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn cpv_fn(a: u8) -> u8 { a }\n\npub fn cpv_extra() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (endpoint, binding_endpoint): (Option<i64>, Option<i64>) = {
        let conn = db.storage.connection();
        let endpoint = conn
            .query_row(
                "SELECT start_logical_symbol_id FROM repo_memory_call_paths WHERE memory_id = ?1",
                params![memory_id],
                |r| r.get(0),
            )
            .unwrap();
        let binding_endpoint = conn
            .query_row(
                "SELECT logical_symbol_id FROM repo_memory_bindings
                  WHERE memory_id = ?1 AND binding_kind = 'call_path'",
                params![memory_id],
                |r| r.get(0),
            )
            .unwrap();
        (endpoint, binding_endpoint)
    };
    assert_eq!(endpoint, None, "a vanished no-winner call-path endpoint must be NULLed");
    assert_eq!(
        binding_endpoint, None,
        "the vanished call_path binding endpoint must be NULLed too"
    );
    // The fake id is truly gone — the assertion above is not just masking a lingering row.
    let fake_rows: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM logical_symbols WHERE id = ?1", params![fake_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fake_rows, 0, "the drifted id vanished (this is the vanished, not occupied, case)");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: `refresh_logical_binding_discriminators` renames each realigned binding's
/// `binding_id` to the live qualified name, but that column is part of the PK. When the SAME
/// memory already carries a sibling logical binding at the target qualified name, the rename
/// collides — a plain `UPDATE OR IGNORE` would silently SKIP the stale row, leaving it on a live
/// id with stale discriminators that validate as current forever. The refresh must delete the
/// stale duplicate, exactly as `validate_memories` does on the same collision shape.
#[test]
fn a_refresh_collision_deletes_the_stale_duplicate_binding() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn dup_anchor(x: u8) -> u8 { x }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let real_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'dup_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    // The live qualified name dup_anchor re-derives to (unchanged across the rebuild) — the
    // target the rename collides on.
    let live_qual: String = db
        .storage
        .connection()
        .query_row(
            "SELECT value FROM name_strings
              WHERE id = (SELECT qualified_name_id FROM logical_symbols WHERE id = ?1)",
            params![real_id],
            |r| r.get(0),
        )
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Bound to dup_anchor under two derivations".to_string(),
            body: "The stale sibling binding must be dropped, not stranded mis-labelled."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(real_id),
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
    drop(db);

    let fake_id: i64 = 424248;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Drift the symbol onto a fake id (qual + signature stored unchanged, so it realigns by
        // evidence onto the re-derived row).
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![fake_id, real_id])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
            params![fake_id, real_id],
        )
        .unwrap();
        // The memory's created binding becomes the DRIFTED one: rename its binding_id to a legacy
        // qual FIRST (freeing the live-qual PK slot), then point it at the fake id.
        conn.execute(
            "UPDATE repo_memory_bindings
                SET binding_id = 'legacy::dup_anchor', logical_symbol_id = ?1
              WHERE memory_id = ?2 AND binding_kind = 'logical_symbol'",
            params![fake_id, memory_id],
        )
        .unwrap();
        // The SIBLING binding already holding the live qualified name (the collision target),
        // also drifted onto the fake id so the remap carries it to the same re-derived row.
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id,
                        logical_symbol_id, anchor_status, created_at_ms)
             VALUES (?1, 'logical_symbol', ?2, ?3, 'current', 0)",
            params![memory_id, live_qual, fake_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn dup_anchor(x: u8) -> u8 { x }\n\npub fn dup_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let fresh_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'dup_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let rows: Vec<(String, i64)> = {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT binding_id, logical_symbol_id FROM repo_memory_bindings
                  WHERE memory_id = ?1 AND binding_kind = 'logical_symbol'
                  ORDER BY binding_id",
            )
            .unwrap();
        stmt.query_map(params![memory_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        rows,
        vec![(live_qual.clone(), fresh_id)],
        "the stale 'legacy::dup_anchor' duplicate must be deleted, leaving one binding at the \
         live qualified name and re-derived id"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: vacating must run BEFORE the remap — an occupied id with no winner can itself be
/// another drifted reference's legitimate target, and a post-remap vacate would wipe the freshly
/// realigned references along with the stale ones.
#[test]
fn vacating_an_occupied_id_does_not_undo_a_remap_landing_on_it() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn undo_alpha(a: u8) -> u8 { a }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let alpha_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'undo_alpha'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let bind_target = |logical: i64| rag_rat_query::memory::RepoMemoryBindTarget {
        logical_symbol_id: Some(logical),
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
    };
    let alpha_memory = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Realigns onto its occupied id".to_string(),
            body: "The vacate of the ghost must not wipe this freshly realigned binding."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: bind_target(alpha_id),
        })
        .unwrap()
        .memory
        .memory_id;
    // The ghost's memory rides the API too (a hand-rolled repo_memories row would fight the
    // full column contract); its binding is re-pointed at the occupied id below.
    let ghost_memory = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "The evidence-dead ghost".to_string(),
            body: "Occupies alpha's re-derived id with a foreign key.".to_string(),
            confidence: "low".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: bind_target(alpha_id),
        })
        .unwrap()
        .memory
        .memory_id;
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Park alpha's row (key intact) on a fake old-derivation id...
        conn.execute("UPDATE logical_symbols SET id = 424242 WHERE id = ?1", params![alpha_id])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = 424242
              WHERE logical_symbol_id = ?1",
            params![alpha_id],
        )
        .unwrap();
        // ...fabricate a GHOST row occupying alpha's real id with evidence-dead key fields (a
        // foreign name — no candidate will ever agree), and point the ghost memory's binding at
        // it so it enters the snapshot.
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('legacy::undo_ghost')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO logical_symbols(id, repo_id, language, path, logical_name,
                                         qualified_name_id, kind, variant_count, group_reason)
             SELECT ?1, repo_id, language, path, 'undo_ghost',
                    (SELECT id FROM name_strings WHERE value = 'legacy::undo_ghost'),
                    kind, 1, 'single'
             FROM logical_symbols WHERE id = 424242",
            params![alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = ?1, binding_id = \
             'legacy::undo_ghost'
              WHERE memory_id = ?2",
            params![alpha_id, ghost_memory],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn undo_alpha(a: u8) -> u8 { a }\n\npub fn undo_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![alpha_memory],
            |r| r.get(0),
        )
        .unwrap();
    let alpha_fresh: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'undo_alpha'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        bound, alpha_fresh,
        "vacating the ghost's stale references must not wipe the remap that landed on the id"
    );
    let ghost_bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![ghost_memory],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(ghost_bound, alpha_fresh, "the ghost's stale reference is off the occupied id");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: two evidence-dead occupied ids carrying `logical_symbol_monikers` rows for the
/// SAME tool must not collide on the moniker PK when vacated — monikers are oracle-derived and
/// re-derivable, so a vacated id's moniker rows are deleted, never collapsed onto one sentinel.
#[test]
fn vacating_two_moniker_bearing_ids_does_not_abort_on_the_moniker_pk() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn pk_alpha(a: u8) -> u8 { a }\n\npub fn pk_beta(b: u16) -> u16 { b }\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let id_of = |db: &IndexDatabase, name: &str| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT id FROM logical_symbols WHERE path = 'src/lib.rs' AND logical_name = ?1",
                [name],
                |r| r.get(0),
            )
            .unwrap()
    };
    let alpha_id = id_of(&db, "pk_alpha");
    let beta_id = id_of(&db, "pk_beta");
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Both rows become evidence-dead occupied ids: legacy quals + legacy member captures, ids
        // swapped so each survives holding the OTHER symbol's re-derived id.
        conn.execute(
            "INSERT OR IGNORE INTO name_strings(value)
             VALUES ('legacy::pk_alpha'), ('legacy::pk_beta')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbols
                SET id = -9001,
                    qualified_name_id = (SELECT id FROM name_strings
                                          WHERE value = 'legacy::pk_alpha')
              WHERE id = ?1",
            params![alpha_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbols
                SET id = ?1,
                    qualified_name_id = (SELECT id FROM name_strings
                                          WHERE value = 'legacy::pk_beta')
              WHERE id = ?2",
            params![alpha_id, beta_id],
        )
        .unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = -9001", params![beta_id])
            .unwrap();
        conn.execute(
            "UPDATE symbols SET signature = 'fn legacy_capture()'
              WHERE id IN (SELECT symbol_id FROM logical_symbol_members)",
            [],
        )
        .unwrap();
        // A moniker row for the SAME tool on each occupied id — the PK-collision shape.
        conn.execute(
            "INSERT INTO logical_symbol_monikers(repo_id, logical_symbol_id, tool,
                                                 tool_version, moniker, computed_at)
             SELECT repo_id, ?1, 'scip-rust', '1', 'legacy::pk_alpha#m', 1
             FROM logical_symbols WHERE id = ?1",
            params![alpha_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO logical_symbol_monikers(repo_id, logical_symbol_id, tool,
                                                 tool_version, moniker, computed_at)
             SELECT repo_id, ?1, 'scip-rust', '1', 'legacy::pk_beta#m', 1
             FROM logical_symbols WHERE id = ?1",
            params![beta_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn pk_alpha(a: u8) -> u8 { a }\n\npub fn pk_beta(b: u16) -> u16 { b }\n\npub fn \
         pk_appendix() {}\n",
    )
    .unwrap();
    // Pre-fix this rebuild ABORTS: both vacates collapse their moniker rows onto one sentinel id
    // and the second UPDATE violates the (repo_id, logical_symbol_id, tool) PK.
    let db = IndexDatabase::rebuild(&config).unwrap();

    let leftover: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM logical_symbol_monikers WHERE logical_symbol_id IN (?1, ?2)",
            params![alpha_id, beta_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover, 0, "vacated ids' moniker rows are deleted, not collapsed or stranded");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a snapshot row whose members were already replaced (an incremental pass deleted
/// them) carries NO signature, so the freshly re-derived row — SAME id, agreeing qualified name —
/// fails the exact-key survivor probe and lands in the occupied set. The evidence pass then names
/// the old id itself as the winner (an IN-PLACE winner, no remap pair), and the vacate sweep must
/// honor that claim: vacating it would rewrite perfectly valid references to the sentinel and
/// delete live monikers on every incremental rebuild that follows a version bump.
#[test]
fn an_in_place_winner_with_dead_snapshot_members_is_not_vacated() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keeper_fn(a: u8) -> u8 { a }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let keeper_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'keeper_fn'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Anchored across a members-dead snapshot".to_string(),
            body: "An in-place evidence winner keeps its references; the vacate sweep must not \
                   wipe them."
                .to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(keeper_id),
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
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute(
            "INSERT INTO logical_symbol_monikers(repo_id, logical_symbol_id, tool,
                                                 tool_version, moniker, computed_at)
             SELECT repo_id, ?1, 'scip-rust', '1', 'keeper::keeper_fn#m', 1
             FROM logical_symbols WHERE id = ?1",
            params![keeper_id],
        )
        .unwrap();
        // The members-dead snapshot shape: the group row survives with its references, but its
        // member rows are gone, so the snapshot recovers NO signature for it.
        conn.execute("DELETE FROM logical_symbol_members WHERE logical_symbol_id = ?1", params![
            keeper_id
        ])
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    // keeper_fn's own content is untouched, so it re-derives to the SAME id; the appendix only
    // defeats the unchanged-content short-circuit.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn keeper_fn(a: u8) -> u8 { a }\n\npub fn keeper_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bound, keeper_id,
        "an in-place winner's binding must stay on its id, not be vacated to the sentinel"
    );
    let monikers: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM logical_symbol_monikers WHERE logical_symbol_id = ?1",
            params![keeper_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(monikers, 1, "an in-place winner's monikers survive the vacate sweep");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a kind-classification bump can leave a DECOY — a same-(path, name,
/// qualified-name) twin still carrying the snapshot row's OLD kind — while the true symbol
/// re-derives under a NEW kind. The strict pass sees only the decoy (its qualified name agrees),
/// but the cross-kind twin agreeing on the axis the decoy lacks (signature) is CONFLICTING
/// evidence: the heal must match nothing and leave the reference for the relocation ladder,
/// never hand it to the decoy.
#[test]
fn a_kind_drift_decoy_with_conflicting_evidence_falls_to_the_ladder() {
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
    let group_sig = |db: &IndexDatabase, id: i64| -> Option<String> {
        db.storage
            .connection()
            .query_row(
                "SELECT CASE WHEN COUNT(*) = COUNT(s.signature)
                              AND COUNT(DISTINCT s.signature) = 1
                             THEN MIN(s.signature) END
                   FROM logical_symbol_members m
                   JOIN symbols s ON s.id = m.symbol_id
                  WHERE m.logical_symbol_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    };
    let struct_sig = group_sig(&db, struct_id);
    assert!(struct_sig.is_some(), "fixture: the struct twin must carry a signature capture");
    assert_ne!(
        struct_sig,
        group_sig(&db, impl_id),
        "fixture: the twins' signature evidence must differ"
    );

    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Anchored to the struct across a kind bump".to_string(),
            body: "The old-kind decoy must not inherit this binding by qualified name alone."
                .to_string(),
            confidence: "high".to_string(),
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
    drop(db);

    // Simulate the kind-drift decoy shape: under the OLD derivation the struct's row carried the
    // kind the impl twin still holds today ('impl'), and a fake id models the whole-key churn.
    // After the rebuild, the strict (old-kind) pass sees ONLY the impl decoy, while the true
    // struct row agrees on qualified name AND signature.
    let fake_id: i64 = 424243;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1, kind = 'impl' WHERE id = ?2", params![
            fake_id, struct_id
        ])
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![fake_id, struct_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = ?1 WHERE memory_id = ?2",
            params![fake_id, memory_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/twin.rs"),
        "pub struct TwinAnchor {\n    pub x: i64,\n}\n\nimpl TwinAnchor {\n    pub fn \
         probe(&self) -> i64 {\n        self.x\n    }\n}\n\npub fn twin_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(bound, impl_id, "the heal must not hand the binding to the old-kind decoy");
    assert_eq!(
        bound, fake_id,
        "conflicting cross-kind evidence must match nothing and leave the reference in place"
    );

    // The relocation ladder — not a heal-time guess — resolves it, preferring the twin whose
    // stored discriminators (symbol kind + signature hash) agree.
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
    assert_eq!(status, "relocated", "the ladder owns the conflicted reference");
    assert_eq!(relocated_id, struct_id, "the ladder's discriminators pick the struct twin");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: the heal's `(repo_id, path, logical_name)` candidate probe runs behind a
/// TRANSIENT index — created for the pass, dropped before it returns — so a moniker-bearing repo
/// (snapshot ≈ whole table) doesn't go quadratic, and ordinary rebuilds don't pay index
/// maintenance on their wholesale DELETE + re-INSERT. This pins the cleanup: a heal-armed
/// rebuild must leave no `logical_symbols_drift_heal_idx` behind.
#[test]
fn the_drift_heal_leaves_no_transient_index_behind() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn tidy_fn(a: u8) -> u8 { a }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let tidy_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'tidy_fn'", [], |r| {
            r.get(0)
        })
        .unwrap();
    // A referenced binding keeps the snapshot non-empty, so the heal actually builds the index.
    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "Keeps the drift snapshot non-empty".to_string(),
        body: "The heal must build AND drop its transient candidate index.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test-agent".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            logical_symbol_id: Some(tidy_id),
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
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();
    drop(db);

    fs::write(
        root.join("src/lib.rs"),
        "pub fn tidy_fn(a: u8) -> u8 { a }\n\npub fn tidy_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let stamp: String = db
        .storage
        .connection()
        .query_row("SELECT value FROM repo_meta WHERE key = 'logical_key_version'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(!stamp.is_empty(), "the heal-armed rebuild must stamp the key version");
    let leftover: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'index' AND name = 'logical_symbols_drift_heal_idx'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover, 0, "the transient heal index must not outlive the heal pass");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a PARTIAL pass (incremental edit sweep) re-parses only the changed files, so it
/// must not stamp `logical_key_version` — untouched files still carry old-derivation symbols
/// whose ids churn only when they are eventually re-parsed, and an early stamp would switch the
/// drift heal off with most of the drift still in the future. The pass still RUNS the heal for
/// whatever drift is visible; only the stamp defers to a whole-corpus pass.
#[test]
fn a_partial_pass_heals_without_stamping_the_key_version() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn part_fn(a: u8) -> u8 { a }\n").unwrap();
    // `index_changed` (the incremental entry) needs a git repo to compute the change set.
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let part_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'part_fn'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Healed by the partial pass".to_string(),
            body: "The incremental sweep realigns visible drift but must not stamp.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(part_id),
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
    drop(db);

    let fake_id: i64 = 424244;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![fake_id, part_id])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![fake_id, part_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = ?1 WHERE memory_id = ?2",
            params![fake_id, memory_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    // The PARTIAL pass: an incremental sweep over the edited file, not a full rebuild.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn part_fn(a: u8) -> u8 { a }\n\npub fn part_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();

    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bound, part_id, "the partial pass still heals the drift it can see");
    let stamp: Option<String> = db
        .storage
        .connection()
        .query_row("SELECT value FROM repo_meta WHERE key = 'logical_key_version'", [], |r| {
            r.get(0)
        })
        .optional()
        .unwrap();
    assert_eq!(
        stamp, None,
        "a partial pass must not stamp the key version — untouched files' drift is still ahead"
    );
    drop(db);

    // The whole-corpus pass is what stamps.
    let db = IndexDatabase::rebuild(&config).unwrap();
    let stamp: Option<String> = db
        .storage
        .connection()
        .query_row("SELECT value FROM repo_meta WHERE key = 'logical_key_version'", [], |r| {
            r.get(0)
        })
        .optional()
        .unwrap();
    assert!(stamp.is_some(), "the full rebuild re-derives every file and stamps");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a realigned reference keeps its bind-time relocation discriminators —
/// `binding_id` (the qualified name), `symbol_kind`, `signature_hash` — unless the heal rewrites
/// them. Validation treats the live id as current and never repairs those fields, so a LATER
/// churn or relocation would search with stale evidence and miss (or mis-pick) the twin. The
/// heal must refresh them from the row the reference now points at.
#[test]
fn a_drift_heal_refreshes_the_binding_discriminators() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn disc_fn(a: u8) -> u8 { a }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let disc_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'disc_fn'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Discriminators refreshed by the heal".to_string(),
            body: "A realigned binding must carry current relocation evidence.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(disc_id),
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
    drop(db);

    // Old-derivation storage: the id is fake, and the binding's discriminators are the OLD
    // derivation's captures — a legacy qualified name, a legacy kind label, a stale signature
    // hash.
    let fake_id: i64 = 424245;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![fake_id, disc_id])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![fake_id, disc_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings
                SET logical_symbol_id = ?1, binding_id = 'legacy::disc_fn',
                    symbol_kind = 'legacy_kind', signature_hash = 'stale-hash'
              WHERE memory_id = ?2",
            params![fake_id, memory_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn disc_fn(a: u8) -> u8 { a }\n\npub fn disc_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (bound, binding_id, symbol_kind, signature_hash): (i64, String, String, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id, binding_id, symbol_kind, signature_hash
               FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(bound, disc_id, "the heal realigns the reference onto the re-derived id");
    let (live_qual, live_kind, live_sig): (String, String, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT (SELECT value FROM name_strings WHERE id = ls.qualified_name_id),
                    ls.kind,
                    (SELECT s.signature FROM logical_symbol_members m
                       JOIN symbols s ON s.id = m.symbol_id
                      WHERE m.logical_symbol_id = ls.id LIMIT 1)
               FROM logical_symbols ls WHERE ls.id = ?1",
            params![disc_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(binding_id, live_qual, "binding_id must be refreshed to the live qualified name");
    assert_eq!(symbol_kind, live_kind, "symbol_kind must be refreshed to the live kind");
    assert_eq!(
        signature_hash,
        rag_rat_base::hash::hex_sha256(live_sig.trim().as_bytes()),
        "signature_hash must be refreshed to the live capture's hash"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: `logical_symbol_monikers` has no FK, so a dangling row can sit at ANY id —
/// including one a drift remap is about to land on (the moniker table survives the wholesale
/// logical rebuild that killed its row). The phase-2 reference move must displace the stale
/// occupant, not abort the whole rebuild transaction on the
/// `(repo_id, logical_symbol_id, tool)` PK.
#[test]
fn a_dangling_moniker_at_the_remap_target_does_not_abort_the_heal() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn mon_fn(a: u8) -> u8 { a }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let mon_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'mon_fn'", [], |r| r.get(0))
        .unwrap();
    drop(db);

    // Old-derivation storage: the row (with its oracle moniker — the durable reference that
    // snapshots it) sits on a fake id, while a DANGLING moniker row for the same tool already
    // occupies the id the heal will realign onto.
    let fake_id: i64 = 424246;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![fake_id, mon_id])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![fake_id, mon_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO logical_symbol_monikers(repo_id, logical_symbol_id, tool,
                                                 tool_version, moniker, computed_at)
             SELECT repo_id, ?1, 'scip-rust', '1', 'live::mon_fn#m', 1
             FROM logical_symbols WHERE id = ?1",
            params![fake_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO logical_symbol_monikers(repo_id, logical_symbol_id, tool,
                                                 tool_version, moniker, computed_at)
             SELECT repo_id, ?1, 'scip-rust', '1', 'dangling::mon_fn#m', 1
             FROM logical_symbols WHERE id = ?2",
            params![mon_id, fake_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn mon_fn(a: u8) -> u8 { a }\n\npub fn mon_appendix() {}\n",
    )
    .unwrap();
    // Pre-fix this rebuild ABORTS: the phase-2 moniker move onto the re-derived id collides
    // with the dangling occupant on the (repo_id, logical_symbol_id, tool) PK.
    let db = IndexDatabase::rebuild(&config).unwrap();

    let monikers: Vec<String> = {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT moniker FROM logical_symbol_monikers WHERE logical_symbol_id = ?1
                 ORDER BY moniker",
            )
            .unwrap();
        stmt.query_map(params![mon_id], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(
        monikers,
        vec!["live::mon_fn#m".to_string()],
        "the realigned moniker displaces the stale dangling occupant"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a partial pass that REPLACES the bound file (the single-file heal, the
/// incremental sweep) deletes its old symbols before the logical rebuild runs — and with them
/// the snapshot's signature evidence. If the qualified name is what drifted, a late snapshot
/// then has NO evidence at all and the reference is stranded forever (the old row is gone, so
/// no later pass can heal it either). The drift snapshot must be captured at PASS ENTRY, before
/// any file mutation.
#[test]
fn drift_evidence_survives_the_partial_pass_that_edits_the_bound_file() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn edit_fn(a: u8) -> u8 { a }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let edit_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'edit_fn'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let created = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Survives the edit-and-heal pass".to_string(),
            body: "Evidence must be snapshotted before the pass replaces the file.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                logical_symbol_id: Some(edit_id),
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

    // Old-derivation storage with a DRIFTED qualified name: the heal must lean on the member
    // signature — evidence that lives in the very symbol rows the file replacement deletes.
    {
        let conn = db.storage.connection();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        let fake_id: i64 = 424247;
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('legacy::edit_fn')", [])
            .unwrap();
        conn.execute(
            "UPDATE logical_symbols
                SET id = ?1,
                    qualified_name_id =
                        (SELECT id FROM name_strings WHERE value = 'legacy::edit_fn')
              WHERE id = ?2",
            params![fake_id, edit_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1
              WHERE logical_symbol_id = ?2",
            params![fake_id, edit_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE repo_memory_bindings SET logical_symbol_id = ?1 WHERE memory_id = ?2",
            params![fake_id, memory_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    }

    // The PARTIAL pass: heal_file replaces the file in place (remove + reindex) and then runs
    // the logical rebuild — the old symbols die mid-pass.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn edit_fn(a: u8) -> u8 { a }\n\npub fn edit_appendix() {}\n",
    )
    .unwrap();
    db.heal_file(Path::new("src/lib.rs")).unwrap();

    let bound: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bound, edit_id,
        "the heal must realign via the signature evidence captured BEFORE the file replacement"
    );
    let stamp: Option<String> = db
        .storage
        .connection()
        .query_row("SELECT value FROM repo_meta WHERE key = 'logical_key_version'", [], |r| {
            r.get(0)
        })
        .optional()
        .unwrap();
    assert_eq!(stamp, None, "the single-file heal is a partial pass and must not stamp");

    let _ = fs::remove_dir_all(&root);
}

/// #493 review: a full rebuild that CARRIES live linked-worktree overlay rows forward re-parses
/// the base scope only — the carried symbols keep their old derivation. Stamping the key
/// version over them would let the next overlay refresh see a current stamp, skip the drift
/// snapshot, and strand the overlay's references. With overlays carried, the stamp must defer;
/// a rebuild with none stamps as usual.
#[test]
fn a_rebuild_carrying_overlays_defers_the_key_version_stamp() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn carry_fn(a: u8) -> u8 { a }\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    let stamp = |db: &IndexDatabase| -> Option<String> {
        db.storage
            .connection()
            .query_row("SELECT value FROM repo_meta WHERE key = 'logical_key_version'", [], |r| {
                r.get(0)
            })
            .optional()
            .unwrap()
    };
    assert!(stamp(&db).is_some(), "the overlay-free rebuild stamps");

    // A live linked-worktree overlay whose rows the next full rebuild will carry forward.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-carry", linked.to_str().unwrap()]);
    fs::write(linked.join("src/lib.rs"), "pub fn carry_fn(a: u8) -> u8 { a + 1 }\n").unwrap();
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the branch edit is indexed as an overlay row");
    // The derivation bump arrives while the overlay is live.
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();
    drop(db);

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        stamp(&db),
        None,
        "a rebuild that carried overlay rows must defer the stamp — their symbols were not \
         re-derived"
    );
    drop(db);

    // Once the overlay rows are gone, the next rebuild's corpus is exactly what it re-parses.
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("DELETE FROM files WHERE commit_sha = ''", []).unwrap();
    }
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert!(stamp(&db).is_some(), "an overlay-free rebuild stamps again");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #493 review: the stamp gate must count ALL rows `carry_forward_live_overlays` moves forward,
/// not just linked-worktree overlays. An OTHER-COMMIT committed leftover (`worktree_id = ''`, a
/// prior HEAD's retained rows) is carried un-reparsed — its `worktree_id = ''` differs from the
/// active checkout's `worktree_id` (the canonical path), and its commit differs from the active
/// HEAD — yet `carried_overlay_worktrees` (which returns only `worktree_id != ''`) excludes it.
/// Gating on `carried_overlays.is_empty()` would stamp a stale key version over those carried
/// rows; gating on the carried-row COUNT defers correctly.
#[test]
fn a_rebuild_carrying_a_committed_leftover_defers_the_stamp() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn leftover_fn(a: u8) -> u8 { a }\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "commit A"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let stamp = |db: &IndexDatabase| -> Option<String> {
        db.storage
            .connection()
            .query_row("SELECT value FROM repo_meta WHERE key = 'logical_key_version'", [], |r| {
                r.get(0)
            })
            .optional()
            .unwrap()
    };
    assert!(stamp(&db).is_some(), "the clean rebuild stamps");
    let repo_id = db.active_repo_id.clone();
    let live =
        rag_rat_db::schema::live_files_generation(db.storage.connection(), &repo_id).unwrap();
    // Seed an other-commit committed leftover (worktree_id = '') at the live generation — the
    // #502 HEAD-move retention shape. Not a worktree overlay, so carried_overlay_worktrees skips
    // it, but carry_forward_live_overlays carries it (its commit differs from the active HEAD and
    // its '' worktree_id differs from the active checkout path).
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files (path, language, kind, sha256, modified_at_ms, generated,
                                indexed_at_ms, indexed_revision, commit_sha, worktree_id,
                                has_test_code, repo_id, generation)
             VALUES ('src/leftover_extra.rs', 'rust', 'source', 'stale', 0, 0, 0, '',
                     'stalecommit0000', '', 0, ?1, ?2)",
            params![repo_id, live],
        )
        .unwrap();
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();
    drop(db);

    // Rebuild (HEAD unchanged): the seeded leftover is carried un-reparsed, so — even though there
    // are NO worktree overlays — the stamp must defer.
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        stamp(&db),
        None,
        "a rebuild carrying an other-commit committed leftover must defer the stamp, not stamp \
         over its un-reparsed rows"
    );

    let _ = fs::remove_dir_all(&root);
}
