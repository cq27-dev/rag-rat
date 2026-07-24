use super::*;

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
