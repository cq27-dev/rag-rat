
use super::*;

fn mem_db() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &rag_rat_db::MigrationHooks::noop()).unwrap();
    c
}

fn set_repo(c: &Connection, repo_id: &str) {
    c.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    c.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [repo_id],
    )
    .unwrap();
}

fn seed_file(c: &Connection, path: &str, repo_id: &str) -> i64 {
    c.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation) VALUES \
         (?1,'rust','source',?2,0,0,'','',?3,0)",
        rusqlite::params![path, format!("sha-{path}"), repo_id],
    )
    .unwrap();
    c.last_insert_rowid()
}

fn seed_memory(c: &Connection, id: &str, repo_id: &str) {
    c.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
         created_at_ms, updated_at_ms, source, memory_version, repo_id) VALUES \
         (?1,'Invariant','t','b','high','active','agent',1,1,'agent','v1',?2)",
        rusqlite::params![id, repo_id],
    )
    .unwrap();
}

fn seed_target(c: &Connection, file_id: i64, name: &str, logical_id: i64) -> i64 {
    let qualified = format!("src/lib.rs::{name}");
    c.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", [&qualified]).unwrap();
    c.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, scope_path, kind, \
         start_byte, end_byte, start_line, end_line) VALUES (?1, 'rust', ?2, (SELECT id FROM \
         name_strings WHERE value = ?3), ?2, 'function', 0, 1, 1, 1)",
        rusqlite::params![file_id, name, qualified],
    )
    .unwrap();
    let symbol_id = c.last_insert_rowid();
    c.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind, \
         variant_count, group_reason) VALUES (?1, 'rust', 'src/lib.rs', ?2, (SELECT id FROM \
         name_strings WHERE value = ?3), 'function', 1, 'exact')",
        rusqlite::params![logical_id, name, qualified],
    )
    .unwrap();
    c.execute(
        "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, end_line) \
         VALUES (?1, ?2, 1, 1)",
        rusqlite::params![logical_id, symbol_id],
    )
    .unwrap();
    symbol_id
}

fn relocation_twin(id: i64, kind: &str, group: i64) -> RelocationTwin {
    RelocationTwin {
        id,
        path: "src/lib.rs".to_string(),
        kind: kind.to_string(),
        signature: None,
        logical_symbol_id: Some(group),
        scope: None,
    }
}

/// On a row its writer marked retargeted, the recorded kind outranks the handle: an in-place
/// rebind from a struct to its impl keeps the struct's handle beside the impl's kind, and
/// crediting it first would put the binding back on the struct.
#[test]
fn on_a_retargeted_row_the_recorded_kind_outranks_the_handle() {
    let binding = RepoMemoryBinding {
        symbol_kind: Some("impl".to_string()),
        logical_symbol_id: Some(7),
        ..call_path_binding("mem", "seq")
    };
    let picked = pick_relocation_twin(
        vec![relocation_twin(1, "struct", 7), relocation_twin(2, "impl", 8)],
        &binding,
        true,
        None,
    );
    assert_eq!(picked.map(|twin| twin.id), Some(2));
}

/// On any other row the handle outranks a contradicting kind: a recorded `struct` beside a
/// handle naming an `enum` is as often this checkout's own history as a retarget, and a
/// same-named struct must not take the memory from the handle's enum.
#[test]
fn on_an_unmarked_row_the_handle_outranks_a_contradicting_kind() {
    let binding = RepoMemoryBinding {
        symbol_kind: Some("struct".to_string()),
        logical_symbol_id: Some(7),
        ..call_path_binding("mem", "seq")
    };
    let picked = pick_relocation_twin(
        vec![relocation_twin(1, "struct", 8), relocation_twin(2, "enum", 7)],
        &binding,
        false,
        None,
    );
    assert_eq!(picked.map(|twin| twin.id), Some(2));
}

/// Where the handle agrees with the kind it still decides: two impls of different traits for
/// one type share the name, the kind and the signature, and only the handle tells them apart.
#[test]
fn a_handle_that_agrees_with_the_kind_still_separates_trait_impl_twins() {
    let binding = RepoMemoryBinding {
        symbol_kind: Some("impl".to_string()),
        logical_symbol_id: Some(8),
        ..call_path_binding("mem", "seq")
    };
    let picked = pick_relocation_twin(
        vec![relocation_twin(1, "impl", 7), relocation_twin(2, "impl", 8)],
        &binding,
        false,
        None,
    );
    assert_eq!(picked.map(|twin| twin.id), Some(2));
}

/// On a retargeted row the author's published scope separates twins that agree on kind and
/// signature — two impls of different traits for one type — and outranks the handle, which
/// names the impl the author left. It never outranks the kind or the signature, and where no
/// candidate carries it the handle still decides.
#[test]
fn on_a_retargeted_row_the_published_scope_breaks_a_kind_and_signature_tie() {
    let scoped = |id: i64, kind: &str, group: i64, scope: &str| RelocationTwin {
        scope: Some(scope.to_string()),
        ..relocation_twin(id, kind, group)
    };
    let binding = RepoMemoryBinding {
        symbol_kind: Some("impl".to_string()),
        logical_symbol_id: Some(7),
        ..call_path_binding("mem", "seq")
    };
    let beta = hex_sha256(b"Twin as Beta");
    let twins =
        || vec![scoped(1, "impl", 7, "Twin as Alpha"), scoped(2, "impl", 8, "Twin as Beta")];
    let pick = |retargeted: bool, scope: Option<&str>| {
        pick_relocation_twin(twins(), &binding, retargeted, scope).map(|twin| twin.id)
    };
    assert_eq!(pick(true, Some(&beta)), Some(2), "the scope names Beta over the Alpha handle");
    assert_eq!(pick(true, Some(&hex_sha256(b"Twin as Gamma"))), Some(1), "unmatched: handle");
    assert_eq!(pick(true, None), Some(1), "no scope published: handle");
    assert_eq!(pick(false, Some(&beta)), Some(1), "an unmarked row never reads the scope");

    let mut struct_binding = binding;
    struct_binding.symbol_kind = Some("struct".to_string());
    let picked = pick_relocation_twin(
        vec![scoped(1, "struct", 8, "Twin as Alpha"), scoped(2, "impl", 9, "Twin as Beta")],
        &struct_binding,
        true,
        Some(&beta),
    );
    assert_eq!(picked.map(|twin| twin.id), Some(1), "the kind outranks the scope");
}

/// The applied-targets codec round-trips the scope and still reads rows written before it.
#[test]
fn applied_targets_decode_both_tuple_shapes() {
    let key = ("symbol".to_string(), "src/lib.rs::Twin".to_string());
    let target = AppliedTarget {
        symbol_kind: Some("impl".to_string()),
        signature_hash: Some("sig".to_string()),
        scope_hash: Some("scope".to_string()),
    };
    let targets: AppliedTargets = [(key.clone(), target.clone())].into_iter().collect();
    let json = encode_applied_targets(&targets).unwrap();
    assert_eq!(decode_applied_targets(Some(&json)).unwrap()[&key], target);

    let legacy = r#"[["symbol","src/lib.rs::Twin","impl","sig"]]"#;
    assert_eq!(decode_applied_targets(Some(legacy)).unwrap()[&key], AppliedTarget {
        scope_hash: None,
        ..target
    });
    assert_eq!(decode_applied_targets(None), None);
    assert_eq!(decode_applied_targets(Some("not json")), None);
}

fn call_path_binding(memory_id: &str, edge_sequence_hash: &str) -> RepoMemoryBinding {
    RepoMemoryBinding {
        memory_id: memory_id.to_string(),
        binding_kind: "call_path".to_string(),
        binding_id: edge_sequence_hash.to_string(),
        resolved_binding_id: None,
        path: None,
        start_line: None,
        end_line: None,
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: None,
        signature_hash: None,
        moniker_tool: None,
        moniker_tool_version: None,
        relocation_reason: None,
        anchor_status: "current".to_string(),
        created_at_ms: 0,
    }
}

/// The row-id fast path answers `current` off a file hash, so it may only be taken when the row
/// still hashes to the identity the binding was made against — under the CURRENT fingerprint.
/// A pre-upgrade digest agreeing proves only that the eight older fields match; the receiver
/// type is not among them, so the call may now reach a different method. Row-id reuse during
/// the graph rebuild is what puts a legacy binding in front of this branch, and the honest
/// answer for it is `relocated`.
#[test]
fn a_legacy_fingerprint_does_not_take_the_edge_id_fast_path() {
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "r");
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, receiver_hint, \
         receiver_type_hint, source_file_id, source_start_line, source_end_line) VALUES \
         ('caller','run','calls_name','exact','recv','Alpha',?1,10,10)",
        [file_id],
    )
    .unwrap();
    let edge_id = c.last_insert_rowid();

    let legacy =
        crate::memory::resolve::legacy_edge_fingerprint(crate::memory::EdgeFingerprintParts {
            path: "src/lib.rs",
            start_line: 10,
            end_line: 10,
            from_name: Some("caller"),
            to_name: Some("run"),
            edge_kind: "calls_name",
            target_qualified_name: None,
            receiver_hint: Some("recv"),
            receiver_type_hint: None,
            callee_logical_symbol_id: None,
        });

    seed_memory(&c, "m1", "r");
    let mut binding = RepoMemoryBinding {
        binding_kind: "edge".to_string(),
        binding_id: legacy,
        resolved_binding_id: None,
        // The reused row id the rebuild handed back.
        edge_id: Some(edge_id),
        ..call_path_binding("m1", "unused")
    };
    binding.memory_id = "m1".to_string();

    assert_eq!(validate_edge_binding(&c, &mut binding).unwrap(), AnchorStatus::Relocated);
}

#[test]
fn edge_binding_with_pre_upgrade_fingerprint_relocates_after_hint_gain() {
    // A binding persisted BEFORE receiver_type_hint existed holds the 8-field fingerprint and
    // a now-dead row id (GRAPH_INDEX_VERSION 12 re-extracts every edge). The re-extracted,
    // source-unchanged call site gained a hint, so its current fingerprint differs — the
    // legacy-format fallback must still find it and relocate, not report `gone`.
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "r");
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, receiver_hint, \
         receiver_type_hint, source_file_id, source_start_line, source_end_line) VALUES \
         ('caller','run','calls_name','exact','recv','Alpha',?1,10,10)",
        [file_id],
    )
    .unwrap();

    let legacy =
        crate::memory::resolve::legacy_edge_fingerprint(crate::memory::EdgeFingerprintParts {
            path: "src/lib.rs",
            start_line: 10,
            end_line: 10,
            from_name: Some("caller"),
            to_name: Some("run"),
            edge_kind: "calls_name",
            target_qualified_name: None,
            receiver_hint: Some("recv"),
            receiver_type_hint: None,
            callee_logical_symbol_id: None,
        });

    seed_memory(&c, "m1", "r");
    let mut binding = RepoMemoryBinding {
        binding_kind: "edge".to_string(),
        binding_id: legacy.clone(),
        resolved_binding_id: None,
        ..call_path_binding("m1", "unused")
    };
    binding.memory_id = "m1".to_string();

    assert_eq!(validate_edge_binding(&c, &mut binding).unwrap(), AnchorStatus::Relocated);
    assert!(binding.edge_id.is_some(), "the relocated binding adopts the live edge row");
    assert_ne!(
        binding.current_binding_id(),
        legacy,
        "the binding converges to the current fingerprint"
    );
    assert_eq!(
        validate_edge_binding(&c, &mut binding).unwrap(),
        AnchorStatus::Current,
        "a compatibility relocation converges instead of repeating forever"
    );

    let mut missing = RepoMemoryBinding {
        binding_kind: "edge".to_string(),
        binding_id: "not-a-fingerprint".to_string(),
        resolved_binding_id: None,
        ..call_path_binding("m1", "unused")
    };
    assert_eq!(validate_edge_binding(&c, &mut missing).unwrap(), AnchorStatus::Gone);

    c.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, edge_sequence_hash, path_summary, \
         created_at_ms) VALUES ('m1', 'legacy-path', 'caller -> run', 0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind, receiver_hint) VALUES ('m1', \
         'legacy-path', 0, ?1, 'caller', 'run', 'calls_name', 'recv')",
        [legacy.as_str()],
    )
    .unwrap();
    let mut call_path = call_path_binding("m1", "legacy-path");
    assert_eq!(
        validate_call_path_binding(&c, &mut call_path).unwrap(),
        AnchorStatus::Relocated,
        "legacy identity proves the site survived but cannot prove its receiver owner"
    );
    // Convergence moves the WHOLE binding, not just its member fingerprints: the key is the
    // hash OF those fingerprints, so a half-migrated row would no longer re-derive its own id,
    // and `call_path_memories_for_crossed` — which looks memories up by the hash it computes
    // from LIVE fingerprints — would never surface this memory again.
    assert_ne!(
        call_path.current_binding_id(),
        "legacy-path",
        "the binding id re-points to the v3 hash"
    );
    let (upgraded, key): (String, String) = c
        .query_row(
            "SELECT edge_fingerprint, edge_sequence_hash FROM repo_memory_call_path_edges
                 WHERE memory_id = 'm1' AND ordinal = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_ne!(upgraded, legacy, "validation converges the stored identity to v3");
    assert_eq!(
        key,
        call_path.current_binding_id(),
        "the edge rows follow the binding to its new key"
    );
    assert_eq!(
        compute_edge_sequence_hash([upgraded.as_str()]),
        call_path.current_binding_id(),
        "the converged binding re-derives its own id from its stored fingerprints"
    );
    let reachable: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_call_paths
                 WHERE memory_id = 'm1' AND edge_sequence_hash = ?1",
            [call_path.current_binding_id()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reachable, 1, "the call-path row is re-keyed too, so nothing is orphaned");
    assert_eq!(
        validate_call_path_binding(&c, &mut call_path).unwrap(),
        AnchorStatus::Current,
        "the converged v3 identity is no longer permanently hint-blind"
    );
}

#[test]
fn callee_retarget_invalidates_edge_and_call_path_bindings() {
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "r");
    let alpha = seed_target(&c, file_id, "Alpha", 11);
    let beta = seed_target(&c, file_id, "Beta", 22);
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, receiver_hint, \
         receiver_type_hint, source_file_id, source_start_line, source_end_line, to_symbol_id) \
         VALUES ('caller','run','calls_name','exact','recv','Worker',?1,10,10,?2)",
        rusqlite::params![file_id, alpha],
    )
    .unwrap();
    let edge_id: i64 = c.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap();
    let original = edge_by_id(&c, edge_id).unwrap().unwrap();

    seed_memory(&c, "m1", "r");
    let mut edge_binding = RepoMemoryBinding {
        memory_id: "m1".to_string(),
        binding_kind: "edge".to_string(),
        binding_id: original.fingerprint.clone(),
        resolved_binding_id: None,
        edge_id: Some(edge_id),
        ..call_path_binding("m1", "unused")
    };
    let path_hash = compute_edge_sequence_hash([original.fingerprint.as_str()]);
    c.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, edge_sequence_hash, path_summary, \
         created_at_ms) VALUES ('m1', ?1, 'caller -> run', 0)",
        [path_hash.as_str()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind, receiver_hint, \
         callee_logical_symbol_id, callee_identity_known) VALUES ('m1', ?1, 0, ?2, 'caller', \
         'run', 'calls_name', 'recv', 11, 1)",
        rusqlite::params![path_hash, original.fingerprint],
    )
    .unwrap();

    c.execute("UPDATE edges_data SET to_symbol_id = ?1 WHERE id = ?2", [beta, edge_id]).unwrap();

    assert_eq!(validate_edge_binding(&c, &mut edge_binding).unwrap(), AnchorStatus::Gone);
    assert_eq!(edge_binding.edge_id, None, "a retargeted row id must not remain attached");
    let mut call_path = call_path_binding("m1", &path_hash);
    assert_eq!(
        validate_call_path_binding(&c, &mut call_path).unwrap(),
        AnchorStatus::Gone,
        "loose relocation must reject a different stable callee"
    );
}

#[test]
fn first_gone_validation_detaches_the_persisted_edge_before_status_downgrades() {
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "r");
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, receiver_hint, \
         receiver_type_hint, source_file_id, source_start_line, source_end_line) VALUES \
         ('caller','run','calls_name','exact','recv','Alpha',?1,10,10)",
        [file_id],
    )
    .unwrap();
    let edge_id = c.last_insert_rowid();
    let fingerprint = edge_by_id(&c, edge_id).unwrap().unwrap().fingerprint;

    seed_memory(&c, "m1", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, start_line, \
         end_line, edge_id, anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','edge',?1,'src/lib.rs',10,10,?2,'current',0,'r')",
        rusqlite::params![fingerprint, edge_id],
    )
    .unwrap();
    assert_eq!(memories_for_edges(&c, &[edge_id], 10).unwrap().len(), 1);

    // Reusing the row for a semantically different receiver invalidates the stored fingerprint
    // while preserving the numeric id that edge recall would otherwise follow.
    c.execute("UPDATE edges SET receiver_type_hint = 'Beta' WHERE id = ?1", [edge_id]).unwrap();

    let report = validate_memories(&c, None).unwrap();
    assert_eq!(report.gone, 1);
    let persisted: (String, Option<i64>, Option<i64>) = c
        .query_row(
            "SELECT anchor_status, downgrade_pending_at_ms, edge_id
                 FROM repo_memory_bindings WHERE memory_id = 'm1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(persisted.0, "current", "the first observation must not downgrade status");
    assert!(persisted.1.is_some(), "the first observation arms status hysteresis");
    assert_eq!(persisted.2, None, "the invalid edge identity detaches immediately");
    assert!(
        memories_for_edges(&c, &[edge_id], 10).unwrap().is_empty(),
        "edge recall must not surface the memory on the replacement edge"
    );

    validate_memories(&c, None).unwrap();
    let confirmed: (String, Option<i64>, Option<i64>) = c
        .query_row(
            "SELECT anchor_status, downgrade_pending_at_ms, edge_id
                 FROM repo_memory_bindings WHERE memory_id = 'm1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(confirmed, ("gone".to_string(), None, None));
}

/// A binding whose memory row is gone — the drain keeps a removed synced memory's bindings for
/// `anchors/1` to carry — is neither counted nor rewritten: this pass relocates portable
/// columns, which would publish for a memory this device cannot show. A missing path would
/// otherwise arm the `gone` downgrade.
#[test]
fn a_binding_without_its_memory_is_left_out_of_validation() {
    let c = mem_db();
    set_repo(&c, "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path,
                    start_line, end_line, anchor_status, created_at_ms, repo_id)
             VALUES ('gone', 'path', 'src/missing.rs', 'src/missing.rs', 1, 1, 'current', 0, 'r')",
        [],
    )
    .unwrap();

    let report = validate_memories(&c, None).unwrap();
    assert_eq!(report.checked, 0, "an orphan binding is not part of the report");
    let persisted: (String, Option<i64>) = c
        .query_row(
            "SELECT anchor_status, downgrade_pending_at_ms
                   FROM repo_memory_bindings WHERE memory_id = 'gone'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(persisted, ("current".to_string(), None), "and is left as it was");
}

/// Once a row is `resolved`, its shadows are this store's view NULL included: a relocation
/// onto a target with no signature must clear that evidence, not leave the author's in force
/// (a same-named twin still carrying it would win the next pick). Unresolved, the authored
/// values show through.
#[test]
fn a_resolved_rows_cleared_shadow_is_its_view_and_an_unresolved_row_shows_the_authored_one() {
    let c = mem_db();
    set_repo(&c, "r");
    seed_memory(&c, "m1", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path,
                    start_line, end_line, symbol_kind, signature_hash, anchor_status,
                    created_at_ms, repo_id)
             VALUES ('m1', 'symbol', 'src/a.rs::run', 'src/a.rs', 3, 9, 'function', 'sig',
                     'current', 0, 'r')",
        [],
    )
    .unwrap();
    let binding = |c: &Connection| memory_by_id(c, "m1").unwrap().unwrap().bindings.remove(0);
    let unresolved = binding(&c);
    assert_eq!(
        (
            unresolved.path.as_deref(),
            unresolved.signature_hash.as_deref(),
            unresolved.resolved_binding_id
        ),
        (Some("src/a.rs"), Some("sig"), None),
    );
    c.execute(
        "UPDATE repo_memory_bindings
                SET resolved = 1, resolved_binding_id = 'src/b.rs::run', resolved_path = \
         'src/b.rs',
                    resolved_start_line = 30, resolved_end_line = 40,
                    resolved_symbol_kind = 'function', resolved_signature_hash = NULL
              WHERE memory_id = 'm1'",
        [],
    )
    .unwrap();
    let resolved = binding(&c);
    assert_eq!(resolved.binding_id, "src/a.rs::run", "the authored identity stays");
    assert_eq!(resolved.resolved_binding_id.as_deref(), Some("src/b.rs::run"));
    assert_eq!(resolved.path.as_deref(), Some("src/b.rs"));
    assert_eq!((resolved.start_line, resolved.end_line), (Some(30), Some(40)));
    assert_eq!(resolved.signature_hash, None, "a cleared shadow is not the authored value");
}

#[test]
fn active_scope_validation_preserves_a_linked_worktree_edge_id_as_pending() {
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/branch.rs", "r");
    c.execute("UPDATE main.files SET worktree_id = 'linked' WHERE id = ?1", [file_id]).unwrap();
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, source_file_id,
                    source_start_line, source_end_line)
             VALUES ('caller', 'branch_only', 'calls_name', 'exact', ?1, 10, 10)",
        [file_id],
    )
    .unwrap();
    let edge_id: i64 = c.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap();
    let fingerprint = edge_by_id(&c, edge_id).unwrap().unwrap().fingerprint;
    seed_memory(&c, "m1", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path,
                    start_line, end_line, edge_id, anchor_status, created_at_ms, repo_id)
             VALUES ('m1', 'edge', ?1, 'src/branch.rs', 10, 10, ?2, 'current', 0, 'r')",
        rusqlite::params![fingerprint, edge_id],
    )
    .unwrap();

    // Model a main-checkout connection: the raw store retains the linked row, while the scoped
    // view used by validation cannot see it.
    c.execute_batch(
        "CREATE TEMP VIEW files AS
                 SELECT * FROM main.files WHERE worktree_id = '';",
    )
    .unwrap();

    let report = validate_memories(&c, None).unwrap();
    assert_eq!((report.pending, report.gone), (1, 0));
    let persisted: (String, Option<i64>, Option<i64>) = c
        .query_row(
            "SELECT anchor_status, downgrade_pending_at_ms, edge_id
                   FROM repo_memory_bindings WHERE memory_id = 'm1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(persisted, ("pending".to_string(), None, Some(edge_id)));
    assert_eq!(
        memories_for_edges(&c, &[edge_id], 10).unwrap().len(),
        1,
        "the linked checkout retains its direct edge recall mapping"
    );
}

/// Convergence rewrites the binding's identity, so it may only run when the recomputed hash
/// describes the SAME call path: every edge must have matched a live edge. An edge that
/// survives only by its loose identity has no live fingerprint to fold in, and re-keying on
/// the remainder would silently redefine which path the memory names.
#[test]
fn a_partly_gone_call_path_keeps_its_legacy_identity() {
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "r");
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, receiver_hint, \
         receiver_type_hint, source_file_id, source_start_line, source_end_line) VALUES \
         ('caller','run','calls_name','exact','recv','Alpha',?1,10,10)",
        [file_id],
    )
    .unwrap();
    let legacy =
        crate::memory::resolve::legacy_edge_fingerprint(crate::memory::EdgeFingerprintParts {
            path: "src/lib.rs",
            start_line: 10,
            end_line: 10,
            from_name: Some("caller"),
            to_name: Some("run"),
            edge_kind: "calls_name",
            target_qualified_name: None,
            receiver_hint: Some("recv"),
            receiver_type_hint: None,
            callee_logical_symbol_id: None,
        });

    seed_memory(&c, "m1", "r");
    c.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, edge_sequence_hash, path_summary, \
         created_at_ms) VALUES ('m1', 'legacy-path', 'caller -> run -> vanished', 0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind, receiver_hint) VALUES ('m1', \
         'legacy-path', 0, ?1, 'caller', 'run', 'calls_name', 'recv')",
        [legacy.as_str()],
    )
    .unwrap();
    // A second hop whose edge no longer exists in any form.
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind) VALUES ('m1', 'legacy-path', 1, \
         'no-such-fingerprint', 'run', 'vanished', 'calls_name')",
        [],
    )
    .unwrap();

    let mut binding = call_path_binding("m1", "legacy-path");
    assert_eq!(validate_call_path_binding(&c, &mut binding).unwrap(), AnchorStatus::Stale);
    assert_eq!(binding.binding_id, "legacy-path", "a partial path keeps its stored identity");
    let stored: String = c
        .query_row(
            "SELECT edge_fingerprint FROM repo_memory_call_path_edges
                 WHERE memory_id = 'm1' AND edge_sequence_hash = 'legacy-path' AND ordinal = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, legacy, "no member is converged while the path is incomplete");
}

/// Two authored call-path bindings of one memory resolving to one legacy hash: the first one
/// validated converges the shared local rows onto the v3 hash and re-points every binding
/// resolving to the legacy one. The second was hydrated before that, so it must re-read its
/// resolution rather than look the rows up under — and stamp back — the hash it was loaded
/// with, which would leave it `gone` for good.
#[test]
fn a_sibling_hydrated_before_its_twin_converged_follows_the_converged_hash() {
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "r");
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, receiver_hint, \
         receiver_type_hint, source_file_id, source_start_line, source_end_line) VALUES \
         ('caller','run','calls_name','exact','recv','Alpha',?1,10,10)",
        [file_id],
    )
    .unwrap();
    let legacy =
        crate::memory::resolve::legacy_edge_fingerprint(crate::memory::EdgeFingerprintParts {
            path: "src/lib.rs",
            start_line: 10,
            end_line: 10,
            from_name: Some("caller"),
            to_name: Some("run"),
            edge_kind: "calls_name",
            target_qualified_name: None,
            receiver_hint: Some("recv"),
            receiver_type_hint: None,
            callee_logical_symbol_id: None,
        });
    seed_memory(&c, "m1", "r");
    c.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, edge_sequence_hash, path_summary, \
         created_at_ms) VALUES ('m1', 'legacy-path', 'caller -> run', 0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind, receiver_hint) VALUES ('m1', \
         'legacy-path', 0, ?1, 'caller', 'run', 'calls_name', 'recv')",
        [legacy.as_str()],
    )
    .unwrap();
    for authored in ["h-a", "h-b"] {
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, anchor_status, \
             created_at_ms, repo_id, resolved, resolved_binding_id) VALUES ('m1', 'call_path', \
             ?1, 'current', 0, 'r', 1, 'legacy-path')",
            [authored],
        )
        .unwrap();
    }

    let report = validate_memories(&c, None).unwrap();
    assert_eq!((report.gone, report.checked), (0, 2), "{report:?}");
    let rows: Vec<(String, String, String)> = c
        .prepare(
            "SELECT binding_id, IIF(resolved, resolved_binding_id, binding_id), anchor_status
                 FROM repo_memory_bindings WHERE memory_id = 'm1' ORDER BY binding_id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].1, "legacy-path", "the first sibling converged: {rows:?}");
    assert_eq!(rows[0].1, rows[1].1, "both siblings resolve to the converged hash: {rows:?}");
    let under_converged: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_call_path_edges
                 WHERE memory_id = 'm1' AND edge_sequence_hash = ?1",
            [rows[0].1.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(under_converged, 1, "the local rows moved once, under the converged hash");
}

#[test]
fn call_path_binding_stops_reading_current_after_receiver_type_hint_repoint() {
    // End-to-end (#567): `recv.run()` starts resolved against `Alpha` (`receiver_type_hint =
    // 'Alpha'`). A memory anchors the call path with the fingerprint captured at that moment.
    // Reindexing then re-points the SAME call site's Rust receiver-type inference to `Beta` —
    // path, span, from_name, to_name, edge_kind, target_qualified_name, and receiver_hint all
    // stay identical, only `receiver_type_hint` changes. Before #567 the fingerprint ignored
    // `receiver_type_hint`, so `validate_call_path_binding` kept reporting `current` against a
    // target it no longer actually resolved to. It must not anymore.
    let c = mem_db();
    set_repo(&c, "r");
    let file_id = seed_file(&c, "src/lib.rs", "r");
    c.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, target_qualified_name, \
         receiver_hint, receiver_type_hint, source_file_id, source_start_line, source_end_line) \
         VALUES ('caller','run','calls_name','exact',NULL,'recv','Alpha',?1,10,10)",
        [file_id],
    )
    .unwrap();
    let edge_id: i64 =
        c.query_row("SELECT id FROM edges WHERE to_name = 'run'", [], |row| row.get(0)).unwrap();
    let edge = call_path_edge_by_id(&c, edge_id).unwrap().unwrap();

    seed_memory(&c, "m1", "r");
    c.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','call_path','hash1',NULL,'current',0,'r')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name, receiver_hint) \
         VALUES ('m1','hash1',0,?1,?2,?3,?4,?5,?6)",
        rusqlite::params![
            edge.fingerprint,
            edge.from_name,
            edge.to_name,
            edge.edge_kind,
            edge.target_qualified_name,
            edge.receiver_hint,
        ],
    )
    .unwrap();

    let mut binding = call_path_binding("m1", "hash1");
    assert_eq!(
        validate_call_path_binding(&c, &mut binding).unwrap(),
        AnchorStatus::Current,
        "unchanged edge validates current"
    );

    c.execute("UPDATE edges SET receiver_type_hint = 'Beta' WHERE id = ?1", [edge_id]).unwrap();

    assert_ne!(
        validate_call_path_binding(&c, &mut binding).unwrap(),
        AnchorStatus::Current,
        "a receiver-type-driven re-resolution must not keep validating current against the stale \
         method target"
    );
}
