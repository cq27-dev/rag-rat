//! `ensure_graph_index_current` runs on the OPEN path, so anything it treats as fatal wedges the
//! database for every later open too — the heal is retried and re-fails forever, and no CLI or MCP
//! call can get in to repair it. These tests pin the states that must NOT be fatal.
//!
//! All three arise on ordinary indexes: a deletion tombstone (every repo that ever removed a
//! file), a row whose path is absent from this checkout (the heal repopulates the repo's whole
//! generation, including sibling commits/worktrees), and a Rust file above the parse limit.

use super::*;

/// Stage an index that owes BOTH heals: the graph-version bump (which drives the reindex loop)
/// and the logical-key bump (which turns on the Rust scope refresh).
fn owe_both_heals(db: &IndexDatabase) {
    db.set_repo_meta("graph_index_version", "0").unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET graph_version = 0, scope_version = 0
             WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'",
            params![&db.active_repo_id, db.active_generation],
        )
        .unwrap();
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();
    // These tests rewind the version meta behind a connection that has already run passes, so a
    // drift snapshot memoized while the version still read CURRENT (i.e. `None`, nothing to heal)
    // can still be pending. A genuine pre-upgrade open carries no memo; drop it so the heal
    // captures the snapshot itself, which is also what decides whether it may stamp.
    *db.drift_snapshot.lock().expect("drift snapshot lock") = None;
}

fn indexed_root(files: &[(&str, &str)]) -> (ScratchRoot, rag_rat_base::config::Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    for (name, body) in files {
        fs::write(root.join("src").join(name), body).unwrap();
    }
    let config = source_config(root.to_path_buf(), Language::Rust);
    (root, config)
}

/// A `kind='deleted'` tombstone carries `language='unknown'` (`mark_file_deleted`). Neither token
/// parses into `Language`/`TargetKind`, so a heal that walks raw `main.files` rows without
/// excluding tombstones fails at the FIRST one — on every open, permanently. Any repo that has
/// ever deleted an indexed file holds these rows.
#[test]
fn a_deletion_tombstone_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Delete an indexed file and let the incremental pass write its tombstone, exactly as a
    // normal edit-and-reindex cycle does.
    fs::write(root.join("src/gone.rs"), "pub fn vanishing() {}\n").unwrap();
    let db = {
        drop(db);
        IndexDatabase::index_discover(&config).unwrap()
    };
    fs::remove_file(root.join("src/gone.rs")).unwrap();
    drop(db);
    let db = IndexDatabase::index_discover(&config).unwrap();
    let tombstones: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE kind = 'deleted'", [], |row| row.get(0))
        .unwrap();
    assert!(tombstones > 0, "the delete-and-reindex cycle must leave a tombstone to test against");

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("a tombstone row must not fail the heal");
    assert_eq!(
        db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "the heal ran to completion and stamped its version"
    );
    // Tombstones are filtered before the loop, so every row this heal walked was covered — the
    // one shape that may stamp the key version and let later passes take the scoped re-derive.
    assert_eq!(
        db.repo_meta("logical_key_version").unwrap().as_deref(),
        Some(LOGICAL_KEY_VERSION),
        "full coverage stamps the key version"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The heal repopulates the repo's whole generation, so it walks rows for paths that need not
/// exist under THIS root (a sibling commit/worktree scope) — and a row can simply outlive its file
/// between an edit and the next index pass. An unreadable path is an expected outcome of that
/// repopulation, not a failure.
#[test]
fn a_file_row_without_a_file_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A live row (not a tombstone) whose path has no file behind it.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                 indexed_at_ms, indexed_revision, commit_sha, worktree_id, repo_id, generation,
                 has_test_code)
             SELECT 'src/absent.rs', 'rust', 'source', '', 0, 0, 0, '', commit_sha, worktree_id,
                    repo_id, generation, 0
             FROM main.files LIMIT 1",
            [],
        )
        .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("an unreadable path must not fail the heal");

    let _ = fs::remove_dir_all(&root);
}

/// Above `MAX_GRAPH_PARSE_BYTES` the indexer persists no symbols at all (the chunker declines at
/// the same bound), so there is nothing for the scope refresh to certify. Treating the file as an
/// un-certifiable heal blocker would fail the open over a file the index never parsed.
#[test]
fn an_oversized_rust_file_does_not_wedge_the_graph_heal() {
    let filler = "pub fn pad() {}\n".repeat(40_000);
    assert!(filler.len() > edges::MAX_GRAPH_PARSE_BYTES, "the fixture must exceed the parse limit");
    let (root, config) = indexed_root(&[
        ("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n"),
        ("huge.rs", filler.as_str()),
    ]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("an oversized Rust file must not fail the heal");

    let _ = fs::remove_dir_all(&root);
}

/// Every `(to_name, resolution)` on `file_id`'s edges — a row the heal must not touch has to keep
/// its RESOLUTIONS too, not merely its edge names. Re-extracting a row the connection's resolve
/// pass will not revisit replaces resolved targets with fresh `unresolved` candidates, which is
/// how a sibling scope silently loses its graph.
fn edge_targets_with_resolution(db: &IndexDatabase, file_id: i64) -> Vec<(String, String)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT n.value, r.value FROM edges_data d
             JOIN name_strings n ON n.id = d.to_name_id
             JOIN name_strings r ON r.id = d.resolution_id
             WHERE d.source_file_id = ?1
             ORDER BY n.value, r.value",
        )
        .unwrap();
    let rows = stmt.query_map([file_id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    rows.map(Result::unwrap).collect()
}

/// The persisted dispatch facts for one file, including the resolution assigned by the graph
/// resolver. Keeping the kind/evidence in this probe makes an upgrade test assert the actual
/// hidden fact row rather than only a user-facing synthesized edge.
fn edge_kind_rows_with_resolution(
    db: &IndexDatabase,
    file_id: i64,
    edge_kind: &str,
) -> Vec<(String, String, Option<String>)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT n.value, r.value, d.evidence
             FROM main.edges_data d
             JOIN main.name_strings n ON n.id = d.to_name_id
             JOIN main.name_strings r ON r.id = d.resolution_id
             JOIN main.name_strings k ON k.id = d.edge_kind_id
             WHERE d.source_file_id = ?1 AND k.value = ?2
             ORDER BY d.id",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![file_id, edge_kind], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn delete_dispatch_handle_facts(db: &IndexDatabase, file_id: i64) {
    db.storage
        .connection()
        .execute(
            r#"DELETE FROM main.edges_data
             WHERE source_file_id = ?1
               AND edge_kind_id = (SELECT id FROM main.name_strings WHERE value = 'dispatch_handle')"#,
            [file_id],
        )
        .unwrap();
}

fn current_graph_version() -> i64 {
    GRAPH_INDEX_VERSION.parse().expect("GRAPH_INDEX_VERSION is an integer")
}

/// The stamp a deployed index carries when the newest `GRAPH_INDEX_VERSION` ladder entry is the one
/// that must re-extract it. DERIVED from the constant, never written out: a hardcoded pair of
/// literals keeps passing when a change forgets to bump the ladder, because the staged version and
/// the expected one then describe a step that already happened rather than this one.
fn previous_graph_version() -> i64 {
    current_graph_version() - 1
}

/// Stage the graph-only part of the version upgrade while leaving the logical-key derivation at
/// its current value.
fn stage_previous_graph_version(db: &IndexDatabase) {
    let previous = previous_graph_version();
    db.set_repo_meta("graph_index_version", &previous.to_string()).unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET graph_version = ?3
             WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'",
            params![&db.active_repo_id, db.active_generation, previous],
        )
        .unwrap();
    *db.drift_snapshot.lock().expect("drift snapshot lock") = None;
}

/// The handler vehicle is a plain in-file free function — the shape whose delegate verdict no
/// classifier rule is contested over, so these upgrade assertions watch the version ladder rather
/// than the classifier. A chain glued onto a constant (`Handler::DEFAULT.run(..)`,
/// `tools::TOOL_NAMES.iter().map(..)`) is an adapter tail and records no handler at all, so it
/// cannot carry a dispatch fact for them to watch (#1124).
fn dispatch_fixture_body(handler_expression: &str) -> String {
    format!(
        r#"
pub enum Msg {{ Work }}

pub fn enqueue() {{ send(Msg::Work); }}
fn send(_msg: Msg) {{}}

pub fn handle(msg: Msg) {{
    match msg {{
        Msg::Work => {handler_expression},
    }}
}}

fn run(_input: usize) -> usize {{ 0 }}
fn elapsed() -> usize {{ 0 }}
"#
    )
}

#[test]
fn a_previous_version_database_reextracts_the_corrected_dispatch_handle_fact() {
    let (root, config) = indexed_root(&[("lib.rs", &dispatch_fixture_body("run(1)"))]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let file_id = scoped_file_id(&db, "src/lib.rs", &db.active_worktree_id);

    // Simulate the deployed row after the old classifier omitted this handle fact.
    let initial_handles = edge_kind_rows_with_resolution(&db, file_id, "dispatch_handle");
    assert!(
        initial_handles.iter().any(|(name, _, evidence)| {
            name == "run" && evidence.as_deref() == Some("Msg::Work")
        }),
        "the fixture must initially persist the corrected dispatch fact: {initial_handles:?}"
    );
    delete_dispatch_handle_facts(&db, file_id);
    assert!(edge_kind_rows_with_resolution(&db, file_id, "dispatch_handle").is_empty());
    stage_previous_graph_version(&db);

    db.ensure_graph_index_current().unwrap();

    let handles = edge_kind_rows_with_resolution(&db, file_id, "dispatch_handle");
    // The re-extracted candidate binds to the chained method defined in the same file, so the
    // upgrade restores a RESOLVED handler rather than a dangling name.
    assert!(
        handles.iter().any(|(name, resolution, evidence)| {
            name == "run"
                && resolution == "target_name_fallback"
                && evidence.as_deref() == Some("Msg::Work")
        }),
        "the stale row must be re-extracted with the corrected handle fact: {handles:?}"
    );
    assert_eq!(file_graph_version(&db, file_id), current_graph_version());
    assert_eq!(db.repo_meta("graph_index_version").unwrap().as_deref(), Some(GRAPH_INDEX_VERSION));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_graph_upgrade_isolated_to_the_active_linked_checkout() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/lib.rs"), dispatch_fixture_body("run(1)")).unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // A TYPE-owned constant chain like the active checkout's, naming a different method so the
    // sibling's fact is distinguishable from the active checkout's.
    fs::write(linked.join("src/lib.rs"), dispatch_fixture_body("elapsed()")).unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch body"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    set_base_scope(&mut db, &main);
    let base_id = scoped_file_id(&db, "src/lib.rs", "");
    let overlay_worktree = worktree_id_of(&linked);
    let overlay_id = scoped_file_id(&db, "src/lib.rs", &overlay_worktree);
    let sibling_resolution =
        edges::intern_edge_string(db.storage.connection(), "sibling_resolution").unwrap();
    db.storage
        .connection()
        .execute(
            r#"UPDATE main.edges_data
             SET resolution_id = ?1
             WHERE source_file_id = ?2
               AND edge_kind_id = (SELECT id FROM main.name_strings WHERE value = 'dispatch_handle')"#,
            params![sibling_resolution, overlay_id],
        )
        .unwrap();
    let overlay_before = edge_kind_rows_with_resolution(&db, overlay_id, "dispatch_handle");
    assert!(
        overlay_before.iter().any(|(name, _, evidence)| {
            name == "elapsed" && evidence.as_deref() == Some("Msg::Work")
        }),
        "the linked checkout starts with its own dispatch fact: {overlay_before:?}"
    );
    let overlay_all_before = edge_targets_with_resolution(&db, overlay_id);
    let overlay_edge_ids_before = edge_ids(&db, overlay_id);

    // Remove only the active checkout's corrected fact, then present both rows as stale data.
    delete_dispatch_handle_facts(&db, base_id);
    stage_previous_graph_version(&db);
    db.ensure_graph_index_current().unwrap();

    let base_handles = edge_kind_rows_with_resolution(&db, base_id, "dispatch_handle");
    assert!(
        base_handles.iter().any(|(name, _, evidence)| {
            name == "run" && evidence.as_deref() == Some("Msg::Work")
        }),
        "the active checkout receives the corrected dispatch fact: {base_handles:?}"
    );
    assert_eq!(file_graph_version(&db, base_id), current_graph_version());
    assert_eq!(file_graph_version(&db, overlay_id), previous_graph_version());
    assert_eq!(edge_ids(&db, overlay_id), overlay_edge_ids_before);
    assert_eq!(edge_targets_with_resolution(&db, overlay_id), overlay_all_before);
    assert_eq!(edge_kind_rows_with_resolution(&db, overlay_id, "dispatch_handle"), overlay_before);
    assert_ne!(db.repo_meta("graph_index_version").unwrap().as_deref(), Some(GRAPH_INDEX_VERSION));

    let mut linked_config = source_config(linked.to_path_buf(), Language::Rust);
    linked_config.database = config.database.clone();
    drop(db);
    let linked_db = IndexDatabase::open_config(&linked_config).unwrap();
    let overlay_handles = edge_kind_rows_with_resolution(&linked_db, overlay_id, "dispatch_handle");
    assert!(
        overlay_handles.iter().any(|(name, _, evidence)| {
            name == "elapsed" && evidence.as_deref() == Some("Msg::Work")
        }),
        "the sibling later re-extracts its own fact: {overlay_handles:?}"
    );
    assert_eq!(file_graph_version(&linked_db, base_id), current_graph_version());
    assert_eq!(file_graph_version(&linked_db, overlay_id), current_graph_version());
    assert_eq!(edge_kind_rows_with_resolution(&linked_db, base_id, "dispatch_handle")[0].0, "run");
    assert_eq!(
        linked_db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION)
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// Every `to_name` on `file_id`'s edges — the file's outgoing graph as EXTRACTED, independent of
/// which scope is active (edge rows are keyed by `source_file_id`, and base and overlay hold
/// separate rows for a shadowed path).
fn edge_target_names(db: &IndexDatabase, file_id: i64) -> Vec<String> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT n.value FROM edges_data d
             JOIN name_strings n ON n.id = d.to_name_id
             WHERE d.source_file_id = ?1
             ORDER BY n.value",
        )
        .unwrap();
    let names = stmt.query_map([file_id], |row| row.get::<_, String>(0)).unwrap();
    names.map(Result::unwrap).collect()
}

fn edge_ids(db: &IndexDatabase, file_id: i64) -> Vec<i64> {
    let mut stmt = db
        .storage
        .connection()
        .prepare("SELECT id FROM main.edges_data WHERE source_file_id = ?1 ORDER BY id")
        .unwrap();
    let ids = stmt.query_map([file_id], |row| row.get::<_, i64>(0)).unwrap();
    ids.map(Result::unwrap).collect()
}

fn file_graph_version(db: &IndexDatabase, file_id: i64) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT graph_version FROM main.files WHERE id = ?1", [file_id], |row| {
            row.get(0)
        })
        .unwrap()
}

fn file_scope_version(db: &IndexDatabase, file_id: i64) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT scope_version FROM main.files WHERE id = ?1", [file_id], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn an_older_binary_does_not_downgrade_future_row_provenance() {
    let (root, config) = indexed_root(&[
        ("future.rs", "pub fn future_helper() {}\npub fn future_entry() { future_helper(); }\n"),
        ("owed.rs", "pub fn owed_helper() {}\npub fn owed_entry() { owed_helper(); }\n"),
    ]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let future_id = scoped_file_id(&db, "src/future.rs", &db.active_worktree_id);
    let owed_id = scoped_file_id(&db, "src/owed.rs", &db.active_worktree_id);
    let future_resolution =
        edges::intern_edge_string(db.storage.connection(), "future_resolution").unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.edges_data SET resolution_id = ?1 WHERE source_file_id = ?2",
            params![future_resolution, future_id],
        )
        .unwrap();
    let future_edges = edge_targets_with_resolution(&db, future_id);
    // Stamp the row with a genuinely FUTURE graph version — the current `GRAPH_INDEX_VERSION`
    // plus one — so the fixture stays ahead of the constant however often it moves (#1124 held
    // feedback: a hard-coded stamp silently became equal to the current version and the test
    // stopped proving anything).
    let future_graph_version = GRAPH_INDEX_VERSION.parse::<i64>().unwrap() + 1;
    db.storage
        .connection()
        .execute(
            "UPDATE main.files SET graph_version = ?2, scope_version = 4 WHERE id = ?1",
            params![future_id, future_graph_version],
        )
        .unwrap();
    db.storage
        .connection()
        .execute("UPDATE main.files SET graph_version = 0, scope_version = 0 WHERE id = ?1", [
            owed_id,
        ])
        .unwrap();
    db.set_repo_meta("graph_index_version", "0").unwrap();
    db.set_repo_meta(LOGICAL_KEY_VERSION_KEY, "4").unwrap();

    db.ensure_graph_index_current().unwrap();

    assert_eq!(file_graph_version(&db, future_id), future_graph_version);
    assert_eq!(file_scope_version(&db, future_id), 4);
    assert_eq!(
        edge_targets_with_resolution(&db, future_id),
        future_edges,
        "resolving an owed sibling must not rewrite future-derived edges"
    );
    assert_eq!(file_graph_version(&db, owed_id), GRAPH_INDEX_VERSION.parse::<i64>().unwrap());
    assert_eq!(file_scope_version(&db, owed_id), 0, "scope healing waits for the newer binary");
    assert_eq!(
        db.repo_meta(LOGICAL_KEY_VERSION_KEY).unwrap().as_deref(),
        Some("4"),
        "the older binary preserves the future global grouping stamp"
    );
    let _ = fs::remove_dir_all(&root);
}

/// The `main.files` row id for `path` in the scope identified by `worktree_id` (`''` = base).
fn scoped_file_id(db: &IndexDatabase, path: &str, worktree_id: &str) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT id FROM main.files WHERE path = ?1 AND worktree_id = ?2 AND repo_id = ?3",
            params![path, worktree_id, &db.active_repo_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// The heal walks the repo's WHOLE generation — every commit/worktree scope — but reads from ONE
/// checkout. A linked worktree's row for a path that also exists in the active checkout must
/// therefore keep ITS OWN graph: re-deriving it from the active root would stamp the base
/// checkout's calls onto the branch's rows, silently corrupting every graph answer served from
/// that worktree. The row's `sha256` is what decides — bytes that do not hash to the row are not
/// that row's source.
#[test]
fn a_graph_heal_does_not_rewrite_a_sibling_worktrees_edges() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("src/target.rs"),
        "pub fn base_helper() {}\npub fn entry() { base_helper(); }\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The branch rewrites the file's body, so base and overlay hold DIFFERENT content for the
    // same path — the one shape a single-root heal cannot serve both of.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(
        linked.join("src/target.rs"),
        "pub fn overlay_helper() {}\npub fn entry() { overlay_helper(); }\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch body"]);
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "target.rs indexed as an overlay row");

    set_base_scope(&mut db, &main);
    let base_id = scoped_file_id(&db, "src/target.rs", "");
    let overlay_id = scoped_file_id(&db, "src/target.rs", &worktree_id_of(&linked));
    assert_ne!(base_id, overlay_id, "base and overlay must hold separate rows for the path");
    let overlay_before = edge_target_names(&db, overlay_id);
    let overlay_resolved_before = edge_targets_with_resolution(&db, overlay_id);
    let base_edge_ids_before = edge_ids(&db, base_id);
    let overlay_edge_ids_before = edge_ids(&db, overlay_id);
    assert!(
        overlay_before.iter().any(|name| name == "overlay_helper"),
        "the overlay row's graph is derived from the BRANCH body: {overlay_before:?}"
    );

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("the heal runs from the base checkout");

    assert_eq!(file_graph_version(&db, base_id), GRAPH_INDEX_VERSION.parse::<i64>().unwrap());
    assert_eq!(file_scope_version(&db, base_id), LOGICAL_KEY_VERSION.parse::<i64>().unwrap());
    assert_eq!(file_graph_version(&db, overlay_id), 0);
    assert_eq!(file_scope_version(&db, overlay_id), 0);
    let base_edge_ids_after = edge_ids(&db, base_id);
    assert_ne!(base_edge_ids_after, base_edge_ids_before, "the active row was re-extracted");
    assert_eq!(edge_ids(&db, overlay_id), overlay_edge_ids_before, "the sibling row was untouched");

    assert_eq!(
        edge_target_names(&db, overlay_id),
        overlay_before,
        "the heal read only the base checkout, so the overlay row's graph must be untouched"
    );
    assert_eq!(
        edge_targets_with_resolution(&db, overlay_id),
        overlay_resolved_before,
        "and its RESOLUTIONS survive — `resolve_edges` writes only rows this connection's view \
         admits, so re-extracting an out-of-view row would strand it as unresolved"
    );
    let base_after = edge_target_names(&db, base_id);
    assert!(
        base_after.iter().any(|name| name == "base_helper"),
        "the active checkout's own row IS re-derived: {base_after:?}"
    );

    assert_ne!(
        db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "the repo summary stays pending while a sibling row is owed"
    );

    let mut linked_config = source_config(linked.to_path_buf(), Language::Rust);
    linked_config.database = config.database.clone();
    drop(db);
    assert!(
        IndexDatabase::try_open_config_read_only(&linked_config).unwrap().is_none(),
        "a read-only sibling open must notice its visible lagging row"
    );
    let linked_db = IndexDatabase::open_config(&linked_config).unwrap();
    assert_eq!(
        edge_ids(&linked_db, base_id),
        base_edge_ids_after,
        "the sibling open leaves the base graph untouched"
    );
    assert_ne!(
        edge_ids(&linked_db, overlay_id),
        overlay_edge_ids_before,
        "the sibling later re-extracts its own unchanged row"
    );
    assert_eq!(
        edge_targets_with_resolution(&linked_db, overlay_id),
        overlay_resolved_before,
        "the refreshed sibling graph still describes the branch body"
    );
    assert_eq!(
        file_graph_version(&linked_db, overlay_id),
        GRAPH_INDEX_VERSION.parse::<i64>().unwrap()
    );
    assert_eq!(
        file_scope_version(&linked_db, overlay_id),
        LOGICAL_KEY_VERSION.parse::<i64>().unwrap()
    );
    assert_eq!(
        linked_db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "the repo summary converges after every live row is current"
    );
    assert_eq!(
        linked_db.repo_meta(LOGICAL_KEY_VERSION_KEY).unwrap().as_deref(),
        Some(LOGICAL_KEY_VERSION),
        "the logical-key summary converges after every scope row is current"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// A row the digest gate skips keeps its OLD `scope_path`, and `regroup_logical_symbols` reads
/// raw `main.files` across every scope — so that stale scope does not merely lag, it changes
/// IDENTITY: the cross-scope group splits, the stale row keeps the original `stable_id`, and the
/// refreshed row gets a new one. Stamping the key version over that would also disarm the two
/// mechanisms built to recover from it (the drift snapshot, and the scoped-re-derive gate, which
/// by construction never revisits untouched rows). So partial coverage must DEFER the stamp.
#[test]
fn partial_coverage_defers_the_logical_key_stamp() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(
        main.join("src/shared.rs"),
        "pub struct S;\nimpl Greet for S { fn hello(&self) {} }\n",
    )
    .unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // A branch whose copy of the path differs, so its row can never be vouched for from here.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(
        linked.join("src/shared.rs"),
        "pub struct S;\nimpl Greet for S { fn hello(&self) {} }\npub fn only_here() {}\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    set_base_scope(&mut db, &main);
    owe_both_heals(&db);
    db.ensure_graph_index_current().unwrap();

    assert_ne!(
        db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "an unrefreshed sibling row keeps the graph summary pending"
    );
    assert_ne!(
        db.repo_meta("logical_key_version").unwrap().as_deref(),
        Some(LOGICAL_KEY_VERSION),
        "a row whose scope could not be refreshed must leave the key version unstamped, so the \
         drift heal and the full-rederive gate stay armed"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// The flip side of the digest gate: it must not turn the heal into a no-op. With a linked
/// worktree's rows in the same database, the ACTIVE checkout's own rows still hash to their
/// source, so the heal re-derives them — a gate that read the wrong column, or compared against
/// a normalized form of the text, would quietly skip everything and leave the graph unbuilt.
/// Full coverage must also STAMP the key version, or every later pass pays a whole-corpus regroup.
#[test]
fn a_graph_heal_still_repopulates_the_active_checkouts_rows() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/shared.rs"), "pub fn helper() {}\npub fn entry() { helper(); }\n")
        .unwrap();
    fs::write(main.join("src/touched.rs"), "pub fn only_on_main() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // The branch touches a DIFFERENT file, so `shared.rs` keeps identical content in both scopes.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/touched.rs"), "pub fn only_on_branch() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    set_base_scope(&mut db, &main);
    let shared_id = scoped_file_id(&db, "src/shared.rs", "");
    db.storage
        .connection()
        .execute("DELETE FROM edges_data WHERE source_file_id = ?1", [shared_id])
        .unwrap();
    assert!(edge_target_names(&db, shared_id).is_empty(), "the row starts the heal with no graph");

    owe_both_heals(&db);
    db.ensure_graph_index_current().unwrap();

    let after = edge_target_names(&db, shared_id);
    assert!(
        after.iter().any(|name| name == "helper"),
        "a content-identical row is re-derived, not skipped: {after:?}"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// A file edited since it was indexed no longer hashes to its row, so the digest gate rejects it
/// before the scope refresh or the edge re-derivation ever run. That is the ordinary state between
/// an edit and the next watcher pass, and it must not fail the open — the row keeps the graph it
/// has and the next index of that file lands the current shape.
#[test]
fn a_file_edited_since_indexing_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let file_id = scoped_file_id(&db, "src/lib.rs", &db.active_worktree_id.clone());
    let before = edge_target_names(&db, file_id);
    assert!(!before.is_empty(), "the row starts with a graph to preserve");

    // Prepend a symbol WITHOUT reindexing: every stored span is now stale by the same offset.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn inserted_ahead_of_everything() {}\npub struct Alpha;\nimpl Alpha { pub fn \
         run(&self) {} }\n",
    )
    .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("a file edited since indexing must not fail the heal");

    assert_eq!(
        edge_target_names(&db, file_id),
        before,
        "a row the gate rejects keeps its edges rather than losing or re-deriving them"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn retrying_an_unverified_row_does_not_reresolve_current_rows() {
    let (root, config) = indexed_root(&[
        ("edited.rs", "pub fn edited_helper() {}\npub fn edited_entry() { edited_helper(); }\n"),
        ("stable.rs", "pub fn stable_helper() {}\npub fn stable_entry() { stable_helper(); }\n"),
    ]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let edited_id = scoped_file_id(&db, "src/edited.rs", &db.active_worktree_id);
    let stable_id = scoped_file_id(&db, "src/stable.rs", &db.active_worktree_id);
    fs::write(
        root.join("src/edited.rs"),
        "pub fn unindexed() {}\npub fn edited_helper() {}\npub fn edited_entry() { \
         edited_helper(); }\n",
    )
    .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().unwrap();
    assert_eq!(file_graph_version(&db, edited_id), 0);
    assert_eq!(file_graph_version(&db, stable_id), GRAPH_INDEX_VERSION.parse::<i64>().unwrap());

    let sentinel = edges::intern_edge_string(db.storage.connection(), "sentinel").unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.edges_data SET resolution_id = ?1 WHERE source_file_id = ?2",
            params![sentinel, stable_id],
        )
        .unwrap();
    let stable_before_retry = edge_targets_with_resolution(&db, stable_id);

    db.ensure_graph_index_current().unwrap();

    assert_eq!(
        edge_targets_with_resolution(&db, stable_id),
        stable_before_retry,
        "retrying the unverified row must not rewrite an already-current sibling"
    );
    let _ = fs::remove_dir_all(&root);
}

/// `refresh_symbol_scopes` reports a shortfall when it cannot reach every persisted symbol of a
/// file — the `unrefreshed` half of the heal's bookkeeping. The digest gate now short-circuits the
/// obvious way in (an edited file), so this reaches it the only remaining way: a persisted symbol
/// row the current parse does not produce. The heal must count it and carry on, NOT fail the open
/// and NOT skip the file's edge re-derivation.
#[test]
fn a_symbol_row_the_parser_cannot_match_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let file_id = scoped_file_id(&db, "src/lib.rs", &db.active_worktree_id.clone());

    // A phantom symbol at a span the file does not contain: the span-matched UPDATE can never
    // reach it, so the refresh covers fewer rows than the file holds.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.symbols(file_id, name, kind, language, qualified_name_id, \
             scope_path,
                 signature, start_line, end_line, start_byte, end_byte)
             SELECT file_id, 'phantom', 'function', language, qualified_name_id, 'phantom',
                    signature, 900, 901, 90000, 90010
             FROM main.symbols WHERE file_id = ?1 LIMIT 1",
            [file_id],
        )
        .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("an unmatchable symbol row must not fail the heal");

    assert_eq!(
        db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "the heal ran to completion and stamped its version"
    );
    assert!(
        !edge_target_names(&db, file_id).is_empty(),
        "the file's edges are still re-derived — the shortfall is reported, not fatal"
    );
    assert_eq!(file_scope_version(&db, file_id), 0, "the failed scope refresh remains owed");
    drop(db);
    let reopened = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(
        file_scope_version(&reopened, file_id),
        0,
        "a later open retries rather than certifying the failed refresh"
    );

    let _ = fs::remove_dir_all(&root);
}

/// A FILE row this build cannot name is skipped so the open does not wedge — but skipping it means
/// its symbols keep the old derivation, and the heal has to say so. If the skip is silent and every
/// other row refreshes, the key stamp lands; once it matches, nothing ever owes that row a
/// re-derivation and its logical ids differ from a fresh index forever.
#[test]
fn a_file_row_this_build_cannot_name_defers_the_key_stamp() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct W;\n\nimpl W {\n    fn go(&self) {}\n}\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    owe_both_heals(&db);
    // A language token from a future build. The row stays live and in scope; only its NAME is
    // unreadable here.
    db.storage
        .connection()
        .execute("UPDATE main.files SET language = 'klingon' WHERE path LIKE '%lib.rs'", [])
        .unwrap();
    drop(db);

    let db = IndexDatabase::open_config(&config).unwrap();
    assert_ne!(
        db.repo_meta(LOGICAL_KEY_VERSION_KEY).unwrap().as_deref(),
        Some(LOGICAL_KEY_VERSION),
        "a row the heal could not read leaves the key version owed, not stamped"
    );

    let _ = fs::remove_dir_all(&root);
}

/// A chunk records the symbol it covers by PATH as well as by id, and the chunk-keyed readers still
/// match on the path. Renaming an impl symbol from its trait to its self type without moving the
/// chunk leaves those lookups searching a name nothing answers to, so an upgraded index loses the
/// chunk associations — and the memories keyed through them — for every trait impl it heals.
#[test]
fn a_heal_that_renames_an_impl_moves_its_chunk_path_with_it() {
    let (root, config) = indexed_root(&[(
        "lib.rs",
        "pub struct W;\npub trait A { fn go(&self); }\n\nimpl A for W {\n    fn go(&self) {}\n}\n",
    )]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // The pre-upgrade shape: the impl symbol named for its TRAIT, and its chunk pointing at that
    // name, exactly as an index built before this change carries them.
    let impl_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM main.symbols WHERE kind = 'impl'", [], |r| r.get(0))
        .unwrap();
    db.storage
        .connection()
        .execute(
            "UPDATE main.chunks SET symbol_path = 'src/lib.rs::A' WHERE symbol_id = ?1",
            params![impl_id],
        )
        .unwrap();
    let touched: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.chunks WHERE symbol_id = ?1 AND symbol_path = \
             'src/lib.rs::A'",
            params![impl_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(touched > 0, "the fixture must stage a chunk on the old trait-based path");
    // A vector already computed from the OLD path, marked servable.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.chunk_embeddings(chunk_id, model_id, model_version, \
             source_text_hash, input_hash, status, vector_blob, created_at_ms)
             SELECT id, 'm', 'v1', 'unchanged', 'stale-input', 'Current', X'', 0
               FROM main.chunks WHERE symbol_id = ?1",
            params![impl_id],
        )
        .unwrap();
    owe_both_heals(&db);
    drop(db);

    let db = IndexDatabase::open_config(&config).unwrap();
    let aligned: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.chunks c
              WHERE c.symbol_id IS NOT NULL
                AND c.symbol_path IS NOT (SELECT ns.value FROM main.symbols s
                                            JOIN main.name_strings ns ON ns.id = \
             s.qualified_name_id
                                           WHERE s.id = c.symbol_id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(aligned, 0, "every linked chunk carries the name its symbol answers to now");

    let served: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.chunk_embeddings WHERE input_hash != ''", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        served, 0,
        "a vector built from the OLD symbol path must stop being served until it is re-embedded"
    );

    let _ = fs::remove_dir_all(&root);
}
