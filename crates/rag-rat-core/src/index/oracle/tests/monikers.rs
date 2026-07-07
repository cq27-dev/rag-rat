use super::*;
// ---------------------------------------------------------------------------
// Moniker anchors (#70, phase 3): oracle-run moniker pass + memory relocation.
// ---------------------------------------------------------------------------
use crate::query::memory::{
    RepoMemoryBindTarget, RepoMemoryCreate, create_memory, doctor_attention_count, memory_by_id,
    validate_memories,
};
/// A definition that maps to an in-corpus symbol writes that logical symbol's moniker; a
/// definition in a document rag-rat never indexed writes nothing.
#[test]
fn oracle_run_writes_monikers_for_in_corpus_defs() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    // A dependency source the `.scip` covers but rag-rat never indexed (on disk, no `files` row).
    std::fs::write(h.root().join("dep.rs"), "fn external_fn() {}\n").unwrap();

    let bytes = scip_bytes_docs(vec![
        // `target` identifier at line 0, chars 3..9.
        ("defs.rs", vec![occurrence(0, 3, 9, TARGET_MONIKER, SymbolRole::Definition as i32)]),
        ("dep.rs", vec![occurrence(
            0,
            3,
            14,
            "rust-analyzer cargo dep 1.0.0 external_fn().",
            SymbolRole::Definition as i32,
        )]),
    ]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    assert_eq!(report.monikers_written, 1, "only the in-corpus def writes a moniker");
    let (moniker, tool, tool_version) = h.moniker(1001).expect("moniker row written");
    assert_eq!(moniker, TARGET_MONIKER);
    assert_eq!(tool, TOOL.as_db_str());
    assert_eq!(tool_version, VERSION);
    let total: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM logical_symbol_monikers", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1, "the unindexed dep def must not write a row");
}

/// A re-run is authoritative for the tool: monikers the current `.scip` no longer defines are
/// cleared, not left stale.
#[test]
fn oracle_rerun_clears_prior_monikers_for_tool() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);

    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert!(h.moniker(1001).is_some());

    // Second run: a `.scip` with no definitions at all.
    let empty = scip_bytes_docs(vec![("defs.rs", vec![])]);
    run_oracle(&h.conn, TOOL, "v2", COMMIT, WORKTREE, &empty, h.root(), None, None).unwrap();
    assert!(h.moniker(1001).is_none(), "authoritative clear removed the stale moniker");
}

/// BEHAVIORAL LIFECYCLE GUARD (#248): the integration test the suite never had. Every prior oracle
/// test wrote+read its outputs with NO reindex in between — which is exactly why the CASCADE-on-
/// reindex bug was invisible for so long. This runs the oracle to populate BOTH oracle-derived
/// outputs (`edge_oracle` via a call/def join, `logical_symbol_monikers` via the def→logical map),
/// then exercises the two reindex shapes that rewrite the volatile parents, and asserts BOTH
/// outputs still resolve through their LIVE-JOIN reads afterward. The two shapes are a FULL reindex
/// (`DELETE FROM edges_data` + a wholesale `logical_symbols` rebuild — DELETE-all + reinsert with
/// the SAME content-derived id, modelling an UNCHANGED symbol) and a per-file INCREMENTAL reindex
/// on an UNCHANGED file (`remove_file_in_scope` deletes + the indexer re-inserts the same edge, the
/// `file_rows.rs` path). It is paired with a CHANGED-file/CHANGED-symbol staleness assertion, so it
/// pins BOTH directions: unchanged content survives, changed content goes stale.
#[test]
fn oracle_outputs_survive_full_and_incremental_reindex() {
    use crate::query::memory::{MonikerResolution, resolve_moniker};

    let h = Harness::new();
    // A caller + a def file. The def's symbol is grouped into a logical symbol (content-derived id
    // 1001 here; stable across rebuilds in production), so the moniker pass writes a moniker for
    // it.
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", target_sym);
    // The heuristic resolved the call; the oracle CONFIRMS it (an in-corpus verdict + a moniker).
    let edge_v1 = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));

    let bytes = scip_bytes_docs(vec![
        ("caller.rs", vec![occurrence(
            0,
            14,
            20,
            TARGET_MONIKER,
            SymbolRole::UnspecifiedSymbolRole as i32,
        )]),
        ("defs.rs", vec![occurrence(0, 3, 9, TARGET_MONIKER, SymbolRole::Definition as i32)]),
    ]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.rows_written, 1, "the run wrote a verdict");
    assert_eq!(report.monikers_written, 1, "the run wrote a moniker");

    // Both outputs resolve through their LIVE-JOIN reads before any reindex (sanity).
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "verdict counted before reindex"
    );
    assert!(
        matches!(
            resolve_moniker(&h.conn, TARGET_MONIKER, TOOL.as_db_str()).unwrap(),
            MonikerResolution::Unique { logical_symbol_id: 1001, .. }
        ),
        "moniker resolves to the live logical symbol before reindex"
    );

    // --- FULL reindex: rewrite edges_data (DELETE + re-insert the SAME edge → new rowid) AND
    // rebuild logical_symbols wholesale (DELETE-all + reinsert the SAME content-derived id 1001,
    // the unchanged-symbol case). The caller/def file shas are untouched. ---
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge_v1]).unwrap();
    let edge_v2 = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    assert_ne!(edge_v2, edge_v1, "full reindex minted a new edge rowid");
    // Wholesale logical_symbols rebuild: drop the members + the logical row, reinsert with the SAME
    // id (unchanged symbol → stable content-derived id).
    h.conn
        .execute("DELETE FROM logical_symbol_members WHERE logical_symbol_id = 1001", [])
        .unwrap();
    h.conn.execute("DELETE FROM logical_symbols WHERE id = 1001", []).unwrap();
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", target_sym);

    // BOTH outputs still resolve after the full reindex (re-anchored by content key / stable id).
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "edge_oracle survives the FULL reindex (re-anchored by content key)"
    );
    assert!(
        matches!(
            resolve_moniker(&h.conn, TARGET_MONIKER, TOOL.as_db_str()).unwrap(),
            MonikerResolution::Unique { logical_symbol_id: 1001, .. }
        ),
        "moniker survives the FULL logical_symbols rebuild (stable content-derived id)"
    );

    // --- INCREMENTAL reindex on the UNCHANGED caller.rs: the per-file path deletes the file's
    // edges then the indexer re-inserts the same one. Model `remove_file_in_scope`'s edge
    // delete + the re-insert; the file row + sha stay put. ---
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge_v2]).unwrap();
    let edge_v3 = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    assert_ne!(edge_v3, edge_v2, "incremental reindex minted a new edge rowid");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "edge_oracle survives the per-file INCREMENTAL reindex of an unchanged file"
    );

    // --- CHANGED-content staleness (the other direction): drift the caller's sha → its verdict
    // goes stale (file_sha mismatch); mint a new logical id for the symbol → its moniker
    // dangles. ---
    h.conn
        .execute("UPDATE files SET sha256 = 'caller-changed' WHERE id = ?1", params![caller])
        .unwrap();
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        0,
        "a CHANGED file's verdict is stale (file_sha mismatch) — not counted"
    );
    // A changed symbol mints a NEW content-derived logical id; the old moniker row dangles.
    h.conn
        .execute("DELETE FROM logical_symbol_members WHERE logical_symbol_id = 1001", [])
        .unwrap();
    h.conn.execute("DELETE FROM logical_symbols WHERE id = 1001", []).unwrap();
    h.add_logical_symbol(2002, "defs.rs", "target", "defs.rs::target", target_sym);
    assert!(
        matches!(
            resolve_moniker(&h.conn, TARGET_MONIKER, TOOL.as_db_str()).unwrap(),
            MonikerResolution::Dangling
        ),
        "a CHANGED symbol's moniker dangles (its content-derived logical id died) — not resolved"
    );
}
/// The #70 acceptance test: a memory bound to a symbol survives a file move (with a content edit
/// the hash fallback can't survive) via moniker relocation — `relocated`, reason `moniker-match`.
#[test]
fn memory_survives_file_move_via_moniker_relocation() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let memory_id = create_target_memory(&h, sym);

    move_target_with_edit(&h, defs, "function");
    // The next oracle run sees the same moniker defined at its new home.
    let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let report = validate_memories(&h.conn, None).unwrap();
    assert!(report.relocated >= 1, "expected a relocation, got {report:?}");

    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let symbol_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "symbol").expect("symbol binding");
    assert_eq!(symbol_binding.anchor_status, "relocated");
    assert_eq!(symbol_binding.binding_id, "moved.rs::target");
    assert_eq!(symbol_binding.path.as_deref(), Some("moved.rs"));
    assert_eq!(symbol_binding.relocation_reason.as_deref(), Some("moniker-match"));
    let moniker_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect("moniker binding");
    assert_eq!(moniker_binding.anchor_status, "relocated");
    assert_eq!(moniker_binding.logical_symbol_id, Some(2002));
}

/// A moniker match under a DIFFERENT current tool_version is lower confidence: it relocates only
/// when the stored `symbol_kind` corroborates the candidate.
#[test]
fn cross_version_moniker_match_requires_kind_corroboration() {
    for (new_kind, expect_status) in [("function", "relocated"), ("struct", "gone")] {
        let h = Harness::new();
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
        h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
        h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
        let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
            0,
            3,
            9,
            TARGET_MONIKER,
            SymbolRole::Definition as i32,
        )])]);
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
        let memory_id = create_target_memory(&h, sym);

        move_target_with_edit(&h, defs, new_kind);
        // The re-run comes from an UPGRADED tool: same moniker, different tool_version.
        let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
            0,
            3,
            9,
            TARGET_MONIKER,
            SymbolRole::Definition as i32,
        )])]);
        run_oracle(&h.conn, TOOL, "v-newer", COMMIT, WORKTREE, &bytes, h.root(), None, None)
            .unwrap();

        validate_memories(&h.conn, None).unwrap();
        let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
        let symbol_binding =
            memory.bindings.iter().find(|b| b.binding_kind == "symbol").expect("symbol binding");
        assert_eq!(
            symbol_binding.anchor_status, expect_status,
            "cross-version match with new_kind={new_kind}"
        );
    }
}

/// #154: a `logical_symbol` binding must stay `current` across a reindex that merely SHIFTS the
/// symbol's lines (an edit elsewhere in the file). The logical symbol's id is content-derived and
/// stable, but chunk ids are reassigned on every re-chunk — so the stored `chunk_id` goes stale.
/// Before the fix the stable-id arm called `validate_bound_chunk`, which found the churned chunk_id
/// missing and returned `gone`; it must instead re-derive the chunk from the live logical symbol.
#[test]
fn logical_symbol_binding_survives_chunk_id_churn_on_reindex() {
    let h = Harness::new();
    let file = h.add_file("a.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(file, "target", "a.rs::target", "function", 0, 14);
    h.add_chunk(file, "a.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "a.rs", "target", "a.rs::target", sym);

    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "target invariant".to_string(),
        body: "target stays reentrant".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { logical_symbol_id: Some(1001), ..Default::default() },
    })
    .unwrap();
    let memory_id = created.memory.memory_id;
    let original_chunk_id = created
        .memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "logical_symbol")
        .expect("logical_symbol binding")
        .chunk_id
        .expect("chunk_id bound");

    // Re-chunk the file: chunk + symbol rows get NEW rowids (as a reindex reassigns them), but the
    // logical symbol keeps its content-derived id 1001 (the symbol is unchanged, just shifted). The
    // content is byte-identical, so the only thing that moved is the chunk_id.
    h.conn.execute("DELETE FROM logical_symbol_members", []).unwrap();
    h.conn.execute("DELETE FROM chunks", []).unwrap();
    h.conn.execute("DELETE FROM symbols", []).unwrap();
    let new_sym = h.add_symbol_qualified(file, "target", "a.rs::target", "function", 0, 14);
    h.add_chunk(file, "a.rs::target", "fn target() {}\n");
    h.conn
        .execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, cfg_expr, \
             signature_hash, start_line, end_line) VALUES (1001, ?1, NULL, NULL, 1, 1)",
            params![new_sym],
        )
        .unwrap();

    validate_memories(&h.conn, None).unwrap();

    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let binding = memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "logical_symbol")
        .expect("logical_symbol binding");
    assert_eq!(
        binding.anchor_status, "current",
        "a logical_symbol binding must survive chunk_id churn on reindex (#154)"
    );
    assert_ne!(
        binding.chunk_id,
        Some(original_chunk_id),
        "the binding's chunk_id should be refreshed to the re-chunked symbol's new chunk"
    );
}

/// The memory body cap is 8000 chars (raised from 4000 so detailed Invariant/Decision/BugPattern
/// memories aren't forced to drop content). Boundary: 8000 accepted, 8001 rejected.
#[test]
fn memory_body_cap_is_8000_chars() {
    let h = Harness::new();
    let make = |body: String| RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "cap test".to_string(),
        body,
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
    };
    assert!(create_memory(&h.conn, make("x".repeat(8000))).is_ok(), "8000 chars is accepted");
    let err = create_memory(&h.conn, make("x".repeat(8001))).unwrap_err();
    assert!(err.to_string().contains("body exceeds 8000"), "8001 rejected with the cap: {err}");
}

/// `doctor_attention_count` (behind the MCP staleness nudge) counts active bindings whose anchor is
/// gone/stale, excludes obsolete memories, and matches the population `memory_doctor` lists.
#[test]
fn doctor_attention_count_counts_active_gone_and_stale_bindings() {
    let h = Harness::new();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "drift test".to_string(),
        body: "x".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
    })
    .unwrap();
    let id = created.memory.memory_id;
    let set_status = |status: &str| {
        h.conn
            .execute(
                "UPDATE repo_memory_bindings SET anchor_status = ?2 WHERE memory_id = ?1",
                params![id, status],
            )
            .unwrap();
    };

    set_status("current");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 0, "current is not counted");
    set_status("gone");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 1, "gone is counted");
    set_status("stale");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 1, "stale is counted");
    // An obsolete memory drops out even with a gone binding.
    h.conn.execute("UPDATE repo_memories SET status = 'obsolete' WHERE id = ?1", [&id]).unwrap();
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 0, "obsolete is excluded");
}

/// The public `memory_attention_count` (the MCP staleness nudge's source) reads from a file DB via
/// a bare read-only open and fails open to 0 on a missing DB — it must never block a tool call.
#[test]
fn memory_attention_count_reads_file_db_and_fails_open() {
    let dir = std::env::temp_dir().join(format!("ragrat-attn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("index.sqlite");

    // Missing DB → 0 (fail-open).
    assert_eq!(crate::memory_attention_count(&db_path), 0, "missing DB is 0, never an error");

    // A real file DB with one gone binding → 1.
    {
        let rw = crate::storage::IndexConnection::open(&db_path).unwrap();
        crate::index::schema::apply(rw.connection()).unwrap();
        let created = create_memory(rw.connection(), RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "drift".to_string(),
            body: "x".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
        })
        .unwrap();
        rw.connection()
            .execute(
                "UPDATE repo_memory_bindings SET anchor_status = 'gone' WHERE memory_id = ?1",
                [&created.memory.memory_id],
            )
            .unwrap();
    }
    assert_eq!(crate::memory_attention_count(&db_path), 1, "counts the gone binding from disk");

    let _ = std::fs::remove_dir_all(&dir);
}
