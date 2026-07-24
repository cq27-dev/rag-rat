use super::*;

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
        // A bare `files` writer must carry the #828 content-digest fold so the delete trigger
        // resolves (a real writer registers it via `IndexConnection::setup`).
        rag_rat_db::content_digest::register_content_digest_fold(&conn).unwrap();
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

/// A chunk resolves to the symbol it actually covers, disambiguated by BYTE OVERLAP (#855).
///
/// `qualified_name` is `"{path}::{simple_name}"` — the bare identifier, not scope-qualified — so
/// every same-simple-name symbol in a file shares one name (`describe` here; `fmt` / `new` /
/// `default` in real code). `chunks.symbol_path` is that same ambiguous string and the chunk
/// carries no symbol id, so resolving on the name alone with `LIMIT 1` returned an arbitrary
/// winner that could flip between reindexes as rowids are reassigned.
///
/// Overlap rather than containment: a chunk begins BEFORE its symbol when it captures the leading
/// doc comment, so `symbol.start <= chunk.start` would reject the right answer.
///
/// NOTE the second half of this test. These two methods have BYTE-IDENTICAL declaration lines, and
/// `signature` — the trimmed first line — is part of `LogicalSymbolKey`, so they collapse into ONE
/// logical symbol. Every logical-id-anchored surface therefore still applies to both, and no
/// chunk-side resolution can change that; it is a defect in the identity key. (When the
/// declaration lines DIFFER the two are distinct logical symbols and the leak is real and fixed —
/// see `a_decision_record_does_not_leak_between_distinct_same_named_methods`.) This test pins the
/// raw-`symbol_id` precision that IS fixable here, plus the collapse itself, so the day the
/// identity key gains a scope discriminator, this test notices.
#[test]
fn a_chunk_binding_resolves_the_symbol_it_overlaps_not_an_arbitrary_same_named_sibling() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct Alpha;\npub struct Beta;\n\nimpl Alpha {\n    /// Alpha's describe.\n    pub fn \
         describe(&self) -> u32 {\n        let a = 1;\n        let b = 2;\n        a + b\n    }\n}\n\nimpl \
         Beta {\n    /// Beta's describe.\n    pub fn describe(&self) -> u32 {\n        let c = \
         3;\n        let d = 4;\n        c + d\n    }\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // The two `describe` symbols share one qualified name and differ only by byte range.
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.start_byte FROM symbols s
             JOIN name_strings ns ON ns.id = s.qualified_name_id
             WHERE ns.value = 'src/lib.rs::describe' ORDER BY s.start_byte",
        )
        .unwrap();
    let describes: Vec<(i64, i64)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
    drop(stmt);
    assert_eq!(describes.len(), 2, "the fixture must actually collide, or this proves nothing");
    let (alpha_symbol, beta_symbol) = (describes[0].0, describes[1].0);

    let chunk_at = |after: i64| -> i64 {
        conn.query_row(
            "SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
             WHERE f.path = 'src/lib.rs' AND c.symbol_path = 'src/lib.rs::describe'
               AND c.start_byte >= ?1 ORDER BY c.start_byte LIMIT 1",
            [after],
            |r| r.get(0),
        )
        .unwrap()
    };
    let alpha_chunk = chunk_at(0);
    let beta_chunk = chunk_at(describes[1].1 - 8);
    assert_ne!(alpha_chunk, beta_chunk, "the two methods must land in distinct chunks");

    let resolve = |chunk_id: i64| {
        rag_rat_query::memory::resolve_binding(conn, &rag_rat_query::memory::RepoMemoryBindTarget {
            chunk_id: Some(chunk_id),
            ..Default::default()
        })
        .unwrap()
        .expect("chunk binding resolves")
    };
    let alpha = resolve(alpha_chunk);
    let beta = resolve(beta_chunk);

    assert_eq!(
        alpha.symbol_id,
        Some(alpha_symbol),
        "the first method's chunk binds the first method, not whichever rowid sorted first",
    );
    assert_eq!(beta.symbol_id, Some(beta_symbol), "and the second method's chunk binds the second",);

    // The known remaining imprecision, pinned deliberately: logical grouping is
    // `(repo_id, path, logical_name)`, so these two distinct methods are ONE logical symbol and
    // anything anchored to it applies to both.
    assert_eq!(
        alpha.logical_symbol_id, beta.logical_symbol_id,
        "same-named symbols in one file still share a logical id — if this ever fails, grouping \
         gained a discriminator and the logical-id-anchored surfaces became per-method",
    );

    let _ = fs::remove_dir_all(&root);
}

/// NESTED same-named symbols: each chunk binds its OWN nesting level (#855).
///
/// A `run` defined inside another `run` is the case that kills the intuitive rule. The inner
/// chunk overlaps the ENCLOSING symbol more than its own — the enclosing range is simply larger —
/// so "widest overlap wins" binds the inner chunk to the outer symbol. Resolution is by closest
/// range instead, which is what makes both directions come out right.
#[test]
fn nested_same_named_symbols_each_bind_their_own_nesting_level() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn run() -> u32 {\n    /// inner\n    fn run() -> u32 {\n        let x = 1;\n        \
         let y = 2;\n        x + y\n    }\n    let z = run();\n    z + 1\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.start_byte, s.end_byte FROM symbols s
             JOIN name_strings ns ON ns.id = s.qualified_name_id
             WHERE ns.value = 'src/lib.rs::run' ORDER BY s.start_byte",
        )
        .unwrap();
    let runs: Vec<(i64, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    drop(stmt);
    assert_eq!(runs.len(), 2, "the fixture must nest two same-named fns");
    let (outer_symbol, inner_symbol) = (runs[0].0, runs[1].0);
    assert!(
        runs[0].1 < runs[1].1 && runs[1].2 < runs[0].2,
        "the second symbol must be nested INSIDE the first, or this tests nothing",
    );

    let mut cstmt = conn
        .prepare(
            "SELECT c.id, c.start_byte FROM chunks c JOIN files f ON f.id = c.file_id
             WHERE f.path = 'src/lib.rs' AND c.symbol_path = 'src/lib.rs::run'
             ORDER BY c.start_byte",
        )
        .unwrap();
    let chunks: Vec<(i64, i64)> =
        cstmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
    drop(cstmt);
    assert_eq!(chunks.len(), 2, "each nesting level gets its own chunk");

    let resolve = |chunk_id: i64| {
        rag_rat_query::memory::resolve_binding(conn, &rag_rat_query::memory::RepoMemoryBindTarget {
            chunk_id: Some(chunk_id),
            ..Default::default()
        })
        .unwrap()
        .expect("chunk binding resolves")
        .symbol_id
    };
    assert_eq!(resolve(chunks[0].0), Some(outer_symbol), "the outer chunk binds the outer fn");
    assert_eq!(
        resolve(chunks[1].0),
        Some(inner_symbol),
        "the inner chunk binds the INNER fn — it overlaps the enclosing symbol more, so a \
         widest-overlap rule would get this backwards",
    );

    let _ = fs::remove_dir_all(&root);
}

/// THE #855 CASE: a decision record must not leak between same-named methods that are genuinely
/// distinct logical symbols.
///
/// Logical grouping keys on `LogicalSymbolKey { language, path, name, qualified_name, kind,
/// signature }`. `qualified_name` is `"{path}::{simple_name}"` and `signature` is the trimmed
/// FIRST LINE of the declaration, so two same-named methods split into distinct logical symbols
/// exactly when their declaration lines differ. When they do — `describe(&self) -> u32` vs
/// `describe(&self, extra: u8) -> u64` here — an arbitrary `LIMIT 1` winner in the chunk resolver
/// binds one method's chunk to the OTHER's logical symbol, and a record anchored there surfaces as
/// context on a method it was never about.
///
/// (Where the declaration lines are byte-identical — two trait impls of `fmt`, two `default`s —
/// the two methods are ONE logical symbol and no chunk-side fix can separate them. That is a
/// distinct defect in the identity key, not something this resolver can reach.)
#[test]
fn a_decision_record_does_not_leak_between_distinct_same_named_methods() {
    use rag_rat_base::config::MemorySurface;
    use rag_rat_query::graph_meta::GraphMetaMode;
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct Alpha;\npub struct Beta;\n\nimpl Alpha {\n    /// Alpha's describe.\n    pub \
         fn describe(&self) -> u32 {\n        let a = 1;\n        let b = 2;\n        a + b\n    \
         }\n}\n\nimpl Beta {\n    /// Beta's describe.\n    pub fn describe(&self, extra: u8) -> \
         u64 {\n        let c = u64::from(extra);\n        let d = 4;\n        c + d\n    }\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.start_byte, m.logical_symbol_id FROM symbols s
             JOIN logical_symbol_members m ON m.symbol_id = s.id
             JOIN name_strings ns ON ns.id = s.qualified_name_id
             WHERE ns.value = 'src/lib.rs::describe' ORDER BY s.start_byte",
        )
        .unwrap();
    let describes: Vec<(i64, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    drop(stmt);
    assert_eq!(describes.len(), 2, "the fixture must collide on name");
    assert_ne!(
        describes[0].2, describes[1].2,
        "differing declaration lines must yield DISTINCT logical symbols, or this test cannot \
         distinguish a leak from correct shared-anchor behaviour",
    );
    let alpha_logical = describes[0].2;

    let chunk_at = |after: i64| -> i64 {
        conn.query_row(
            "SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
             WHERE f.path = 'src/lib.rs' AND c.symbol_path = 'src/lib.rs::describe'
               AND c.start_byte >= ?1 ORDER BY c.start_byte LIMIT 1",
            [after],
            |r| r.get(0),
        )
        .unwrap()
    };
    let (alpha_chunk, beta_chunk) = (chunk_at(0), chunk_at(describes[1].1 - 8));
    assert_ne!(alpha_chunk, beta_chunk);

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
         VALUES ('github','o/r','issue','5','symbol',?1,'describe',1,0,1,?2)",
        rusqlite::params![rag_rat_base::serde_big_id::format_sym_handle(alpha_logical), repo_id],
    )
    .unwrap();

    let records_on = |chunk_id: i64| -> Vec<String> {
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
        .collect()
    };

    assert_eq!(records_on(alpha_chunk), vec!["5".to_string()], "attaches to the anchored method");
    assert!(
        records_on(beta_chunk).is_empty(),
        "and does NOT leak onto the same-named sibling — the wrong decision record presented as \
         context is exactly what #855 reported",
    );

    let _ = fs::remove_dir_all(&root);
}

/// #810: a logical-id remap must carry the distill anchor's `sym_<hex>` TEXT token with it.
///
/// Every other durable reference to a logical id is an INTEGER column, so a remap written as "move
/// the id columns" silently skipped `papertrail_distill_anchors.logical_symbol_id`, which stores
/// the OPAQUE handle as TEXT. The anchor then still names the OLD id, and if a later re-derive
/// hands that id to a different symbol — exactly what the drift heal exists to handle — the anchor
/// resolves to the new occupant and surfaces one symbol's decision record on another.
///
/// The anchor is the ONLY reference here on purpose. Closing the remap alone would not have been
/// enough: the drift snapshot is bounded to ids holding a durable reference, and its definition of
/// that did not include anchors either, so this id would never have been considered for healing.
/// Both halves are required and this fixture fails if either is missing.
#[test]
fn a_remap_heals_a_logical_id_referenced_only_by_a_distill_anchor() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn drift_anchor() -> u32 { 7 }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = rag_rat_db::schema::active_repo_id(db.storage.connection()).unwrap();
    let real_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'drift_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();

    let stale_id = 424242_i64;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![
            stale_id, real_id
        ])
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
            params![stale_id, real_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  root_issue, fix_edge_source, thread_shape, anchors_qualified_count,
                  distilled_at_ms, repo_id)
             VALUES \
             ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
            params![repo_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
                  resolved, candidate_ordinal, selected, repo_id)
             VALUES ('github','o/r','issue','5','symbol',?1,'drift_anchor',1,0,1,?2)",
            params![rag_rat_base::serde_big_id::format_sym_handle(stale_id), repo_id],
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
    let conn = db.storage.connection();

    let fresh_id: i64 = conn
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'drift_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_ne!(
        fresh_id, stale_id,
        "the re-derive must mint a different id, or nothing is remapped"
    );
    let anchor_token: String = conn
        .query_row(
            "SELECT logical_symbol_id FROM papertrail_distill_anchors WHERE item_key = '5'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        anchor_token,
        rag_rat_base::serde_big_id::format_sym_handle(fresh_id),
        "the anchor's sym_<hex> token follows the symbol; left stale it names whatever occupies \
         the old id next",
    );

    let _ = fs::remove_dir_all(&root);
}

/// The same gap, for `repo_node_edges.target_logical_symbol_id` — an INTEGER column that was simply
/// never added to either the remap or the snapshot's reference set. Referenced only by that edge.
#[test]
fn a_remap_heals_a_logical_id_referenced_only_by_a_node_edge() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn drift_anchor() -> u32 { 7 }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = rag_rat_db::schema::active_repo_id(db.storage.connection()).unwrap();
    let real_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'drift_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();

    let stale_id = 424242_i64;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![
            stale_id, real_id
        ])
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
            params![stale_id, real_id],
        )
        .unwrap();
        // FKs are off here, so a placeholder source node is fine — the row exists only to carry a
        // `target_logical_symbol_id` through the remap.
        conn.execute(
            "INSERT INTO repo_node_edges
                 (edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                  target_anchor, target_logical_symbol_id, anchor_status, created_at_ms)
             VALUES ('edge-810', ?1, 'node-810', 'references', ?1, 'symbol', 'drift_anchor', ?2,
                     'current', 0)",
            params![repo_id, stale_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    fs::write(
        root.join("src/lib.rs"),
        "pub fn drift_anchor() -> u32 { 7 }\n\npub fn drift_appendix() {}\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let fresh_id: i64 = conn
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'drift_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_ne!(fresh_id, stale_id);
    let edge_target: i64 = conn
        .query_row(
            "SELECT target_logical_symbol_id FROM repo_node_edges WHERE edge_key = 'edge-810'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(edge_target, fresh_id, "the node-edge target follows the symbol");

    let _ = fs::remove_dir_all(&root);
}

/// No drift winner on an OCCUPIED id: the references must be CLEARED, never left naming it.
///
/// Snapshotting these tables put them in scope for the successful-remap branch. Leaving them out of
/// the failure branch would be worse than never snapshotting them: an occupied no-winner id belongs
/// to a DIFFERENT re-derived symbol, so a reference still naming it resolves to that unrelated
/// symbol while reporting itself current — the false positive #810 calls the concerning case.
///
/// Note the cleanup is deliberately occupied-only. A VANISHED id resolves to nothing, so leaving a
/// reference on it is a miss rather than a mis-attribution, and the validate-time ladder still gets
/// a chance to relocate it.
#[test]
fn a_no_winner_drift_on_an_occupied_id_clears_the_anchor_and_the_node_edge() {
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
    let repo_id = rag_rat_db::schema::active_repo_id(db.storage.connection()).unwrap();
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
    drop(db);

    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // The swap: under OLD rules alpha's key hashed to what is beta's id under the NEW rules.
        // Park alpha's row and its references there; beta will re-derive back onto that id, so it
        // is OCCUPIED after the rebuild.
        conn.execute("DELETE FROM logical_symbol_members WHERE logical_symbol_id = ?1", params![
            beta_id
        ])
        .unwrap();
        conn.execute("DELETE FROM logical_symbols WHERE id = ?1", params![beta_id]).unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![
            beta_id, alpha_id
        ])
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
            params![beta_id, alpha_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  root_issue, fix_edge_source, thread_shape, anchors_qualified_count,
                  distilled_at_ms, repo_id)
             VALUES \
             ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
            params![repo_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
                  resolved, candidate_ordinal, selected, repo_id)
             VALUES ('github','o/r','issue','5','symbol',?1,'alpha_occ',1,0,1,?2)",
            params![rag_rat_base::serde_big_id::format_sym_handle(beta_id), repo_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_node_edges
                 (edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                  target_anchor, target_logical_symbol_id, anchor_status, created_at_ms)
             VALUES ('edge-810', ?1, 'node-810', 'references', ?1, 'symbol', 'alpha_occ', ?2,
                     'current', 0)",
            params![repo_id, beta_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    // alpha disappears, so the parked row has no candidate to match: no winner, and the id it sits
    // on is re-derived back to beta.
    fs::write(root.join("src/lib.rs"), "pub fn beta_occ(b: u16) -> u16 { b }\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    assert_eq!(id_of(&db, "beta_occ"), beta_id, "beta re-derives onto the contested id");

    let anchor: (Option<String>, i64) = conn
        .query_row(
            "SELECT logical_symbol_id, resolved FROM papertrail_distill_anchors WHERE item_key = \
             '5'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        anchor.0, None,
        "the anchor token is cleared rather than left resolving to the symbol that now holds that \
         id",
    );
    assert_eq!(anchor.1, 0, "and the anchor is marked unresolved");

    let edge: (Option<i64>, String) = conn
        .query_row(
            "SELECT target_logical_symbol_id, anchor_status FROM repo_node_edges WHERE edge_key = \
             'edge-810'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(edge.0, None, "the node-edge target is cleared");
    assert_eq!(edge.1, "gone", "and reports itself gone rather than current");

    let _ = fs::remove_dir_all(&root);
}

/// The VANISHED no-winner case: nothing occupies the old id, and the references are still cleared.
///
/// Nothing mis-resolves here — a dead id resolves to nothing — but both of these carry a STATUS
/// next to the reference, and left alone they keep asserting `resolved = 1` and
/// `anchor_status = 'current'` for a target that no longer exists. Their readers act on those
/// fields, so a dead-but-"current" row is a false claim rather than a harmless miss. Memory
/// bindings deliberately behave differently and are left for the relocation ladder.
#[test]
fn a_no_winner_drift_on_a_vanished_id_also_clears_the_anchor_and_the_node_edge() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn drift_anchor() -> u32 { 7 }\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = rag_rat_db::schema::active_repo_id(db.storage.connection()).unwrap();
    let real_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols WHERE logical_name = 'drift_anchor'", [], |r| {
            r.get(0)
        })
        .unwrap();

    let stale_id = 424242_i64;
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![
            stale_id, real_id
        ])
        .unwrap();
        conn.execute(
            "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
            params![stale_id, real_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  root_issue, fix_edge_source, thread_shape, anchors_qualified_count,
                  distilled_at_ms, repo_id)
             VALUES \
             ('github','o/r','issue','5','sha256:h',3,'5','provider','investigation',1,10,?1)",
            params![repo_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
                  resolved, candidate_ordinal, selected, repo_id)
             VALUES ('github','o/r','issue','5','symbol',?1,'drift_anchor',1,0,1,?2)",
            params![rag_rat_base::serde_big_id::format_sym_handle(stale_id), repo_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_node_edges
                 (edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                  target_anchor, target_logical_symbol_id, anchor_status, created_at_ms)
             VALUES ('edge-810v', ?1, 'node-810v', 'references', ?1, 'symbol', 'drift_anchor', ?2,
                     'current', 0)",
            params![repo_id, stale_id],
        )
        .unwrap();
        conn.execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", []).unwrap();
    }

    // The symbol disappears and nothing re-derives onto 424242, so the id simply vanishes.
    fs::write(root.join("src/lib.rs"), "pub fn something_else() {}\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    let occupied: i64 = conn
        .query_row("SELECT COUNT(*) FROM logical_symbols WHERE id = ?1", [stale_id], |r| r.get(0))
        .unwrap();
    assert_eq!(occupied, 0, "the old id must be VANISHED, not occupied, for this test to differ");

    let anchor: (Option<String>, i64) = conn
        .query_row(
            "SELECT logical_symbol_id, resolved FROM papertrail_distill_anchors WHERE item_key = \
             '5'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(anchor.0, None, "a dead token is cleared");
    assert_eq!(anchor.1, 0, "and the anchor stops claiming to be resolved");

    let edge: (Option<i64>, String) = conn
        .query_row(
            "SELECT target_logical_symbol_id, anchor_status FROM repo_node_edges WHERE edge_key = \
             'edge-810v'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(edge.0, None, "a dead target is cleared");
    assert_eq!(edge.1, "gone", "and stops claiming to be current");

    let _ = fs::remove_dir_all(&root);
}
