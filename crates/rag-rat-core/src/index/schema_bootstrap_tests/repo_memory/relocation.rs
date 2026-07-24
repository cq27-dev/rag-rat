use super::*;

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
