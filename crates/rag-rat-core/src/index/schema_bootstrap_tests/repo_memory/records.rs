use super::*;

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

/// #705 drive-by: `read_chunk` attaches the distilled decision records for the symbol the chunk
/// defines — resolved from the chunk's direct `symbol_id` link to the same facet-gated (provider
/// fix edge + a selected symbol anchor), capped, labeled lane the other symbol surfaces use. Rides
/// the memories include flag.
#[test]
fn read_chunk_attaches_distilled_records_for_the_chunk_symbol() {
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
    let logical = symbol.logical_symbol_id.expect("target has a logical id");

    let conn = db.storage.connection();
    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    // A distilled record for issue #5: a PROVIDER fix edge, anchored to `target`'s logical symbol
    // and SELECTED — the two facets `records_for_symbol` gates the drive-by on.
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              root_issue, fix_edge_source, thread_shape, anchors_qualified_count,
              distilled_at_ms, repo_id)
         VALUES ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
        rusqlite::params![repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
             (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
              resolved, candidate_ordinal, selected, repo_id)
         VALUES ('github','o/r','issue','5','symbol',?1,'target',1,0,1,?2)",
        rusqlite::params![rag_rat_base::serde_big_id::format_sym_handle(logical), repo_id],
    )
    .unwrap();

    let chunk_id: i64 = conn
        .query_row(
            "SELECT chunks.id FROM chunks JOIN files ON files.id = chunks.file_id WHERE \
             files.path = 'src/lib.rs' AND chunks.symbol_path IS NOT NULL LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Happy path: the record surfaces on read_chunk, labeled unreviewed.
    let chunk = db
        .read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            true,
            MemorySurface::Full,
        )
        .unwrap()
        .expect("chunk");
    assert_eq!(
        chunk.distilled_records.iter().map(|r| r.record.item_key.as_str()).collect::<Vec<_>>(),
        vec!["5"],
        "the provider-edge, selected-anchor record for this chunk's symbol attaches",
    );
    assert!(chunk.distilled_records[0].unreviewed, "the drive-by record is labeled unreviewed");

    // include_memories = false skips the whole drive-by lane, same as memories.
    let bare = db
        .read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            false,
            MemorySurface::Full,
        )
        .unwrap()
        .expect("chunk");
    assert!(bare.distilled_records.is_empty(), "the drive-by rides the memories include flag");

    // The chunk carries the DIRECT symbol_id link written at index time; a chunk_id with no row
    // (so no reachable symbol) surfaces nothing.
    let linked_symbol_id: Option<i64> = conn
        .query_row("SELECT symbol_id FROM chunks WHERE id = ?1", [chunk_id], |r| r.get(0))
        .unwrap();
    assert!(linked_symbol_id.is_some(), "the code chunk is linked to its defining symbol");
    assert!(
        db.records_for_chunk_symbol(-1, 2).unwrap().is_empty(),
        "a nonexistent chunk surfaces nothing"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #855/#860: two same-simple-name methods in one file with DIFFERENT signatures (`make` on two
/// impls, differing arity) are DISTINCT logical symbols that share a bare `qualified_name`
/// (`src/lib.rs::make`). Each chunk carries a DIRECT `symbol_id` to the exact method it was cut
/// from, so a record anchored to ONE surfaces only on THAT method's chunk — never on the other
/// same-named overload, and never dropped.
#[test]
fn drive_by_records_disambiguate_same_name_overloads_by_symbol_id() {
    use rag_rat_base::config::MemorySurface;
    use rag_rat_query::graph_meta::GraphMetaMode;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct A;\npub struct B;\nimpl A {\n    pub fn make(_x: u8) -> A {\n        A\n    \
         }\n}\nimpl B {\n    pub fn make(_x: u8, _y: u8) -> B {\n        B\n    }\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let conn = db.storage.connection();
    // The two `make` methods share symbol_path `src/lib.rs::make` but are distinct logical symbols
    // (differing arity). Follow each chunk's DIRECT `symbol_id` to its method; order by start_byte
    // for a stable first/second. The join through `chunks.symbol_id` also proves the link is
    // populated at index time — a chunk with a NULL symbol_id would not appear.
    let mut stmt = conn
        .prepare(
            "SELECT chunks.id, members.logical_symbol_id
             FROM chunks
             JOIN symbols ON symbols.id = chunks.symbol_id
             JOIN logical_symbol_members members ON members.symbol_id = symbols.id
             WHERE chunks.symbol_path = 'src/lib.rs::make'
             ORDER BY chunks.start_byte",
        )
        .unwrap();
    let makes: Vec<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(stmt);
    assert_eq!(
        makes.len(),
        2,
        "both `make` overloads carry a symbol_id to their own method: {makes:?}"
    );
    let (first_chunk, first_logical) = makes[0];
    let (second_chunk, second_logical) = makes[1];
    assert_ne!(first_logical, second_logical, "differing signatures ⟹ distinct logical symbols");

    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              root_issue, fix_edge_source, thread_shape, anchors_qualified_count, distilled_at_ms, \
         repo_id)
         VALUES ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
        rusqlite::params![repo_id],
    )
    .unwrap();
    // Anchor the record to ONLY the FIRST `make` overload's logical id.
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
             (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
              resolved, candidate_ordinal, selected, repo_id)
         VALUES ('github','o/r','issue','5','symbol',?1,'make',1,0,1,?2)",
        rusqlite::params![rag_rat_base::serde_big_id::format_sym_handle(first_logical), repo_id],
    )
    .unwrap();

    let read = |chunk_id: i64| {
        db.read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            true,
            MemorySurface::Full,
        )
        .unwrap()
        .expect("chunk")
        .distilled_records
        .iter()
        .map(|r| r.record.item_key.clone())
        .collect::<Vec<_>>()
    };
    // Direct resolution: the record surfaces on the anchored overload's chunk ONLY. Before #855
    // both chunks resolved via the shared symbol_path to one arbitrary `LIMIT 1` winner, so the
    // record would appear on the wrong overload (or the other assertion would fail).
    assert_eq!(read(first_chunk), vec!["5".to_string()], "record on the anchored overload's chunk");
    assert!(read(second_chunk).is_empty(), "the other same-named overload must NOT carry it");

    let _ = fs::remove_dir_all(&root);
}

/// #855/#860: NESTED same-name symbols (a `fn wrap` inside a `fn wrap`) share a bare qualified_name
/// AND their byte/line spans NEST (inner ⊂ outer), so no position metric can disambiguate them.
/// The chunker cuts one chunk per symbol and stamps each with that symbol's DIRECT `symbol_id`, so
/// the outer chunk resolves to the outer fn and the inner chunk to the inner — never crossed.
#[test]
fn drive_by_records_disambiguate_nested_same_name_symbols() {
    use rag_rat_base::config::MemorySurface;
    use rag_rat_query::graph_meta::GraphMetaMode;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn wrap() -> u8 {\n    fn wrap() -> u8 {\n        7\n    }\n    wrap()\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // Both `wrap` symbols share symbol_path; the OUTER has the wider span (it contains the nested
    // inner), and so does its chunk. Order by width DESC so [0] is the outer.
    let logicals: Vec<i64> = conn
        .prepare(
            "SELECT members.logical_symbol_id FROM symbols
             JOIN logical_symbol_members members ON members.symbol_id = symbols.id
             JOIN files ON files.id = symbols.file_id
             WHERE files.path = 'src/lib.rs'
               AND symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = \
             'src/lib.rs::wrap')
             ORDER BY symbols.end_byte - symbols.start_byte DESC",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let chunks: Vec<i64> = conn
        .prepare(
            "SELECT chunks.id FROM chunks JOIN files ON files.id = chunks.file_id
             WHERE files.path = 'src/lib.rs' AND chunks.symbol_path = 'src/lib.rs::wrap'
             ORDER BY chunks.end_byte - chunks.start_byte DESC",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(logicals.len(), 2, "outer + nested wrap are distinct logical symbols: {logicals:?}");
    assert_eq!(chunks.len(), 2, "outer + nested wrap chunks: {chunks:?}");
    assert_ne!(logicals[0], logicals[1]);
    let (outer_logical, outer_chunk, inner_chunk) = (logicals[0], chunks[0], chunks[1]);

    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              root_issue, fix_edge_source, thread_shape, anchors_qualified_count, distilled_at_ms, \
         repo_id)
         VALUES ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
        rusqlite::params![repo_id],
    )
    .unwrap();
    // Anchor the record to the OUTER `wrap` only.
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
             (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
              resolved, candidate_ordinal, selected, repo_id)
         VALUES ('github','o/r','issue','5','symbol',?1,'wrap',1,0,1,?2)",
        rusqlite::params![rag_rat_base::serde_big_id::format_sym_handle(outer_logical), repo_id],
    )
    .unwrap();

    let read = |chunk_id: i64| {
        db.read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            true,
            MemorySurface::Full,
        )
        .unwrap()
        .expect("chunk")
        .distilled_records
        .iter()
        .map(|r| r.record.item_key.clone())
        .collect::<Vec<_>>()
    };
    assert_eq!(
        read(outer_chunk),
        vec!["5".to_string()],
        "the OUTER wrap's chunk resolves to the outer (anchored) symbol"
    );
    assert!(
        read(inner_chunk).is_empty(),
        "the NESTED inner wrap must NOT carry the outer's record"
    );

    let _ = fs::remove_dir_all(&root);
}

/// #855/#860: a SPLIT outer function (>120 lines, so it has continuation chunks) whose continuation
/// SPANS a NESTED same-named inner function of a different signature. EVERY part of the outer —
/// part 0 and each `wrap#<n>` continuation — is cut from the outer symbol and carries its DIRECT
/// `symbol_id`, so the continuation binds to the OUTER. A position metric would wrongly pull it to
/// the tiny inner it spans (the inner's span is nearer / narrower); the direct link cannot.
#[test]
fn drive_by_records_bind_a_split_continuation_to_its_outer_symbol_over_a_nested_inner() {
    use rag_rat_base::config::MemorySurface;
    use rag_rat_query::graph_meta::GraphMetaMode;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Outer `wrap(_x: u8)` is > 120 lines (splits into `wrap` + `wrap#1`); the nested `wrap()` (no
    // arg ⟹ different signature ⟹ distinct logical symbol) sits past line 120, inside the `wrap#1`
    // continuation.
    let padding = "    let _a = 0u8;\n".repeat(125);
    fs::write(
        root.join("src/lib.rs"),
        format!(
            "pub fn wrap(_x: u8) -> u8 {{\n{padding}    fn wrap() -> u8 {{ 9 }}\n    let _b = \
             wrap();\n    _x\n}}\n"
        ),
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // Outer = the wrap symbol with the WIDER line span (it contains the nested inner).
    let outer_logical: i64 = conn
        .query_row(
            "SELECT members.logical_symbol_id FROM symbols
             JOIN logical_symbol_members members ON members.symbol_id = symbols.id
             JOIN files ON files.id = symbols.file_id
             WHERE files.path = 'src/lib.rs'
               AND symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = \
             'src/lib.rs::wrap')
             ORDER BY symbols.end_line - symbols.start_line DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // A CONTINUATION chunk of the outer (`wrap#<n>`), which spans the nested inner.
    let cont_chunk: i64 = conn
        .query_row(
            "SELECT chunks.id FROM chunks JOIN files ON files.id = chunks.file_id
             WHERE files.path = 'src/lib.rs' AND chunks.symbol_path LIKE 'src/lib.rs::wrap#%' \
             LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              root_issue, fix_edge_source, thread_shape, anchors_qualified_count, distilled_at_ms, \
         repo_id)
         VALUES ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
        rusqlite::params![repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
             (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
              resolved, candidate_ordinal, selected, repo_id)
         VALUES ('github','o/r','issue','5','symbol',?1,'wrap',1,0,1,?2)",
        rusqlite::params![rag_rat_base::serde_big_id::format_sym_handle(outer_logical), repo_id],
    )
    .unwrap();

    let recs = db
        .read_chunk_with_graph_and_memories(
            cont_chunk,
            GraphMetaMode::Full,
            20,
            true,
            MemorySurface::Full,
        )
        .unwrap()
        .expect("chunk")
        .distilled_records
        .iter()
        .map(|r| r.record.item_key.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        recs,
        vec!["5".to_string()],
        "the outer's continuation chunk resolves to the OUTER symbol, not the nested inner it \
         spans",
    );

    let _ = fs::remove_dir_all(&root);
}

/// #855/#860: two same-name different-signature symbols on ONE physical line (minified / generated
/// code) share start/end line AND byte span, so NO position metric can attribute a chunk to one of
/// them — the case the earlier line/byte resolvers could only make deterministic, never correct.
/// The direct `symbol_id` link solves it: the chunker cuts a chunk per symbol and stamps each with
/// its own symbol's id, so each `f` chunk resolves to ITS OWN method and a record anchored to one
/// surfaces only there.
#[test]
fn drive_by_records_resolve_same_line_symbols_by_symbol_id() {
    use rag_rat_base::config::MemorySurface;
    use rag_rat_query::graph_meta::GraphMetaMode;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two `f` (different signatures ⟹ distinct logical symbols) on ONE physical line.
    fs::write(
        root.join("src/lib.rs"),
        "pub struct A;pub struct B;impl A{fn f()->u8{1}}impl B{fn f(_x:u8)->u8{2}}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // Both `f` chunks share span and symbol_path; each is told apart ONLY by its DIRECT symbol_id.
    // Order by the symbol's start_byte for a stable first (A::f) / second (B::f).
    let mut stmt = conn
        .prepare(
            "SELECT chunks.id, members.logical_symbol_id
             FROM chunks
             JOIN symbols ON symbols.id = chunks.symbol_id
             JOIN logical_symbol_members members ON members.symbol_id = symbols.id
             WHERE chunks.symbol_path = 'src/lib.rs::f'
             ORDER BY symbols.start_byte",
        )
        .unwrap();
    let f_symbols: Vec<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(stmt);
    assert_eq!(
        f_symbols.len(),
        2,
        "both same-line `f` symbols carry a chunk with its own symbol_id: {f_symbols:?}"
    );
    let (first_chunk, first_logical) = f_symbols[0];
    let (second_chunk, second_logical) = f_symbols[1];
    assert_ne!(first_logical, second_logical, "differing signatures ⟹ distinct logical symbols");
    assert_ne!(first_chunk, second_chunk, "each same-line symbol is cut into its OWN chunk");

    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              root_issue, fix_edge_source, thread_shape, anchors_qualified_count, distilled_at_ms, \
         repo_id)
         VALUES ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
        rusqlite::params![repo_id],
    )
    .unwrap();
    // Anchor the record to the FIRST `f` (A::f) only.
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
             (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
              resolved, candidate_ordinal, selected, repo_id)
         VALUES ('github','o/r','issue','5','symbol',?1,'f',1,0,1,?2)",
        rusqlite::params![rag_rat_base::serde_big_id::format_sym_handle(first_logical), repo_id],
    )
    .unwrap();

    let read = |chunk_id: i64| {
        db.read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            true,
            MemorySurface::Full,
        )
        .unwrap()
        .expect("chunk")
        .distilled_records
        .iter()
        .map(|r| r.record.item_key.clone())
        .collect::<Vec<_>>()
    };
    // Each same-line `f` chunk resolves to its OWN method: the record surfaces on A::f's chunk
    // only.
    assert_eq!(read(first_chunk), vec!["5".to_string()], "record on the anchored same-line `f`");
    assert!(read(second_chunk).is_empty(), "the other same-line `f` must NOT carry it");

    let _ = fs::remove_dir_all(&root);
}

/// #855/#860 regression: upgrading an EXISTING index to V083 must not silently drop drive-by
/// records. Pre-migration chunks have NULL `symbol_id`, and the direct-only resolver would return
/// nothing for them — indefinitely, since incremental indexing skips unchanged files. The V083
/// backfill repairs those chunks. This drives the whole seam: prove the record shows, NULL out
/// symbol_id to SIMULATE a pre-V083 index (the record must vanish — so the test disagrees with any
/// stale fallback), run the backfill, and prove the record returns.
#[test]
fn drive_by_records_survive_a_v083_upgrade_via_the_backfill() {
    use rag_rat_base::config::MemorySurface;
    use rag_rat_query::graph_meta::GraphMetaMode;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn target() -> u8 {\n    7\n}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let logical: i64 = conn
        .query_row(
            "SELECT members.logical_symbol_id FROM symbols
             JOIN logical_symbol_members members ON members.symbol_id = symbols.id
             JOIN files ON files.id = symbols.file_id
             WHERE files.path = 'src/lib.rs'
               AND symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = \
             'src/lib.rs::target')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let chunk_id: i64 = conn
        .query_row(
            "SELECT chunks.id FROM chunks JOIN files ON files.id = chunks.file_id
             WHERE files.path = 'src/lib.rs' AND chunks.symbol_path = 'src/lib.rs::target'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              root_issue, fix_edge_source, thread_shape, anchors_qualified_count, distilled_at_ms, \
         repo_id)
         VALUES ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
        rusqlite::params![repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
             (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
              resolved, candidate_ordinal, selected, repo_id)
         VALUES ('github','o/r','issue','5','symbol',?1,'target',1,0,1,?2)",
        rusqlite::params![rag_rat_base::serde_big_id::format_sym_handle(logical), repo_id],
    )
    .unwrap();

    let read = || {
        db.read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            true,
            MemorySurface::Full,
        )
        .unwrap()
        .expect("chunk")
        .distilled_records
        .iter()
        .map(|r| r.record.item_key.clone())
        .collect::<Vec<_>>()
    };

    // Freshly indexed: the chunk carries an exact symbol_id and the record surfaces.
    assert_eq!(read(), vec!["5".to_string()], "record surfaces on the freshly indexed chunk");

    // Simulate a pre-V083 index: strip the direct link the migration had not yet added.
    conn.execute("UPDATE chunks SET symbol_id = NULL", []).unwrap();
    assert!(read().is_empty(), "a pre-V083 chunk (NULL symbol_id) resolves to no record");

    // The V083 backfill re-links the pre-migration chunk by line-range containment.
    rag_rat_db::schema::apply_chunk_symbol_id(conn).unwrap();
    assert_eq!(read(), vec!["5".to_string()], "the backfill restores the record on the old chunk");

    let _ = fs::remove_dir_all(&root);
}

/// #705 drive-by: the semantic_search enrichment attaches each hit's symbol's distilled records
/// (the last of the four drive-by surfaces). The enrichment pass — NOT the shared
/// `search_with_graph_meta` — attaches them, so docs_for_symbol and other search consumers do not.
#[test]
fn semantic_search_record_enrichment_does_not_ride_the_shared_search() {
    use crate::index::SearchRequest;
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
    let logical = symbol.logical_symbol_id.expect("target has a logical id");

    let conn = db.storage.connection();
    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              root_issue, fix_edge_source, thread_shape, anchors_qualified_count,
              distilled_at_ms, repo_id)
         VALUES ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
        rusqlite::params![repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
             (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
              resolved, candidate_ordinal, selected, repo_id)
         VALUES ('github','o/r','issue','5','symbol',?1,'target',1,0,1,?2)",
        rusqlite::params![rag_rat_base::serde_big_id::format_sym_handle(logical), repo_id],
    )
    .unwrap();

    // The shared search does NOT attach records — that stays on the semantic_search handler, so
    // docs_for_symbol and other `search_with_graph_meta` consumers never surface them.
    let mut hits = db.search_with_graph_meta(SearchRequest::new("target", 10)).unwrap();
    assert!(
        hits.iter().all(|h| h.distilled_records.is_empty()),
        "the shared search does not attach records: {hits:?}",
    );
    assert!(
        hits.iter().any(|h| h.symbol_path.is_some()),
        "the target hit is present with a symbol to enrich: {hits:?}",
    );

    // The semantic_search enrichment attaches the target's record to the matching hit.
    db.attach_distilled_records_to_search_hits(&mut hits).unwrap();
    assert!(
        hits.iter().any(|h| h.distilled_records.iter().any(|r| r.record.item_key == "5")),
        "the target hit carries its distilled record after enrichment: {hits:?}",
    );

    let _ = fs::remove_dir_all(&root);
}
