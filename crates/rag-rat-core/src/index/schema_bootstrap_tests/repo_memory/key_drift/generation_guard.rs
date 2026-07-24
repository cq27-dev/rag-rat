use super::*;

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
