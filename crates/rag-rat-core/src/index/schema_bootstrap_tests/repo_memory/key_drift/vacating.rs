use super::*;

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
