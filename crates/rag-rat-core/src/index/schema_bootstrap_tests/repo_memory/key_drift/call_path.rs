use super::*;

/// A call path can be the ONLY durable reference to its resolved callee. Drift discovery must see
/// that id or the rebuild drops the old logical row before the remap protocol ever considers it.
#[test]
fn a_callee_only_call_path_reference_enters_the_drift_snapshot() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn callee() {}\npub fn caller() { callee(); }\n")
        .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let edge_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT id FROM edges WHERE to_name = 'callee' AND to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "Callee-only drift reference".to_string(),
        body: "No endpoint or symbol binding also names the callee.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test-agent".to_string()),
        source: Some("agent".to_string()),
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget {
            edge_path: Some(vec![edge_id]),
            ..Default::default()
        },
    })
    .unwrap();
    let callee_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT callee_logical_symbol_id FROM repo_memory_call_path_edges", [], |row| {
            row.get(0)
        })
        .unwrap();
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();

    let snapshot = db
        .logical_key_drift_snapshot()
        .unwrap()
        .expect("a missing key-version stamp arms drift discovery");
    assert!(
        snapshot.iter().any(|row| row.old_id == callee_id),
        "the call-path edge's callee id must independently make the logical row durable",
    );
    let _ = fs::remove_dir_all(&root);
}

/// Logical ids are repo-wide while the connection's `files` view is checkout-scoped. A remap from
/// the main checkout must still rebuild a call-path identity authored against a linked overlay.
#[test]
fn a_remap_uses_linked_worktree_edge_evidence_outside_the_active_scope() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn callee() {}\npub fn caller() { callee(); }\n")
        .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "call-path", linked.to_str().unwrap()]);
    fs::write(linked.join("src/lib.rs"), "\npub fn callee() {}\npub fn caller() { callee(); }\n")
        .unwrap();
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    let linked_edge: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT edges.id FROM edges JOIN files ON files.id = edges.source_file_id
              WHERE edges.to_name = 'callee' AND edges.to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let memory_id = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Linked call path".to_string(),
            body: "Its exact line identity exists only in the linked overlay.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                edge_path: Some(vec![linked_edge]),
                ..Default::default()
            },
        })
        .unwrap()
        .memory
        .memory_id;
    let (old_callee, old_fingerprint, old_hash): (i64, String, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT callee_logical_symbol_id, edge_fingerprint, edge_sequence_hash
               FROM repo_memory_call_path_edges WHERE memory_id = ?1",
            [&memory_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    set_base_scope(&mut db, &main);
    let new_callee = old_callee + 1;
    rag_rat_query::memory::remap_call_path_callee_logical_symbol_ids(
        db.storage.connection(),
        db.storage.connection(),
        &[(old_callee, Some(new_callee))],
    )
    .unwrap();
    let (callee, fingerprint, edge_hash, path_hash, binding_hash): (
        i64,
        String,
        String,
        String,
        String,
    ) = db
        .storage
        .connection()
        .query_row(
            "SELECT e.callee_logical_symbol_id, e.edge_fingerprint, e.edge_sequence_hash,
                    p.edge_sequence_hash, b.binding_id
               FROM repo_memory_call_path_edges e
               JOIN repo_memory_call_paths p ON p.memory_id = e.memory_id
               JOIN repo_memory_bindings b ON b.memory_id = e.memory_id
                  AND b.binding_kind = 'call_path'
              WHERE e.memory_id = ?1",
            [&memory_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert_eq!(callee, new_callee);
    assert_ne!(fingerprint, old_fingerprint, "the hidden overlay edge supplied exact evidence");
    assert_ne!(edge_hash, old_hash);
    assert_eq!(edge_hash, path_hash);
    assert_eq!(path_hash, binding_hash);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
