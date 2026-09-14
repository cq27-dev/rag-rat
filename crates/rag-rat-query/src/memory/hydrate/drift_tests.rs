use super::*;
use crate::memory::api::{memories_for_chunk, memories_for_path, memories_for_symbol};
use crate::memory::fixtures::{self, MemorySeed};

const REPO: &str = "r";

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [REPO],
    )
    .unwrap();
    conn
}

/// A file with one chunk whose current text hashes to `text_hash`.
fn seed_chunk(conn: &Connection, path: &str, text_hash: &str) -> i64 {
    let file_id = fixtures::seed_file(conn, path, REPO);
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
         text_hash) VALUES (?1,'code',0,10,1,5,?2)",
        params![file_id, text_hash],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// A memory carrying `stamp` as its author-stamped hash, anchored to `chunk_id`.
fn seed_memory(conn: &Connection, id: &str, origin: &str, stamp: Option<&str>, chunk_id: i64) {
    fixtures::seed_memory(conn, MemorySeed {
        id,
        repo_id: REPO,
        origin,
        source_text_hash: stamp,
        ..MemorySeed::default()
    });
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, chunk_id, \
         anchor_status, created_at_ms, repo_id) VALUES \
         (?1,'chunk',?2,'src/a.rs',?3,'current',0,?4)",
        params![id, chunk_id.to_string(), chunk_id, REPO],
    )
    .unwrap();
}

#[test]
fn a_synced_memory_whose_anchor_text_moved_on_is_demoted_but_still_surfaces() {
    let conn = db();
    // The checkout now holds `now`; the author stamped `then`.
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", Some("then"), chunk);

    let found = memories_for_chunk(&conn, chunk, 10).unwrap();
    assert_eq!(found.len(), 1, "drift must never hide a memory");
    assert!(found[0].synced_anchor_drifted, "a diverged stamp marks");

    let (direct, stale) = split_active_stale(found);
    assert!(direct.is_empty(), "a drifted anchor does not present as confidently current");
    assert_eq!(stale.len(), 1, "it is demoted into the stale lane, not dropped");
}

#[test]
fn a_synced_memory_still_anchored_to_its_stamped_text_is_untouched() {
    let conn = db();
    // What a content-confirmed relocation leaves behind: the anchor moved, the text did not,
    // so the stamp still names what is there.
    let chunk = seed_chunk(&conn, "src/a.rs", "same");
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", Some("same"), chunk);

    let found = memories_for_chunk(&conn, chunk, 10).unwrap();
    assert!(!found[0].synced_anchor_drifted, "a matching stamp is not drift");
    assert_eq!(split_active_stale(found).0.len(), 1, "and it stays in the direct lane");
}

#[test]
fn a_synced_memory_carrying_no_stamp_surfaces_unmarked() {
    let conn = db();
    // Every pre-carrier row is NULL. Absence of a stamp is not evidence of drift.
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", None, chunk);

    let found = memories_for_chunk(&conn, chunk, 10).unwrap();
    assert!(!found[0].synced_anchor_drifted, "a NULL stamp cannot diverge");
    assert_eq!(split_active_stale(found).0.len(), 1);
}

#[test]
fn a_local_memory_in_identical_drift_is_not_marked() {
    let conn = db();
    // Same divergence as the marked case, authored locally. Local drift is relocation's job;
    // marking it here would demote most of a living repo's own memories.
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "local", Some("then"), chunk);

    let found = memories_for_chunk(&conn, chunk, 10).unwrap();
    assert!(!found[0].synced_anchor_drifted, "the rule is scoped to synced rows");
    assert_eq!(split_active_stale(found).0.len(), 1);
}

#[test]
fn a_path_anchor_is_priced_by_its_files_hash() {
    let conn = db();
    // `resolve_path_binding` stamps a path anchor from `files.sha256`, so this checkout can
    // price it — and must. Leaving path anchors out of the candidate set would keep a peer's
    // path-anchored memory presenting as current however far its file had moved on.
    seed_chunk(&conn, "src/a.rs", "irrelevant");
    install_files_view(&conn, "");
    fixtures::seed_memory(&conn, MemorySeed {
        repo_id: REPO,
        origin: "synced",
        source_text_hash: Some("stamped-then"),
        ..MemorySeed::default()
    });
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, start_line, \
         end_line, anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/a.rs:1-5','src/a.rs',1,5,'current',0,?1)",
        params![REPO],
    )
    .unwrap();

    let found = memories_for_path(&conn, "src/a.rs", 10).unwrap();
    assert_eq!(found.len(), 1, "still surfaces");
    assert!(found[0].synced_anchor_drifted, "the file's hash is not what the author stamped");
}

#[test]
fn an_anchor_this_checkout_does_not_serve_leaves_the_memory_unmarked() {
    let conn = db();
    // The anchored file is not in this checkout's view at all, so nothing here can speak to
    // the memory either way. Absence of evidence is not evidence of drift.
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    seed_memory(&conn, "m1", "synced", Some("then"), chunk);
    conn.execute_batch(
        "DROP VIEW IF EXISTS temp.files;
             CREATE TEMP VIEW temp.files AS SELECT * FROM main.files WHERE 0",
    )
    .unwrap();

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(!memory.synced_anchor_drifted, "an unserved anchor yields no verdict");
}

#[test]
fn an_overlay_shadows_the_base_row_it_overrides() {
    let conn = db();
    // A linked worktree overrides the path. The scoped view hides the base row, so the stamp
    // must be judged against the OVERLAY text this checkout serves — matching the hidden base
    // hash is exactly the false "current" a `main.files` read would produce.
    seed_chunk_in(&conn, "src/a.rs", "base-text", "");
    conn.execute("UPDATE main.files SET sha256 = 'stamped-base' WHERE worktree_id = ''", [])
        .unwrap();
    seed_chunk_in(&conn, "src/a.rs", "overlay-text", "wt-active");
    conn.execute("UPDATE main.files SET sha256 = 'moved-on' WHERE worktree_id = 'wt-active'", [])
        .unwrap();
    set_active_worktree(&conn, "wt-active");
    install_files_view(&conn, "wt-active");

    fixtures::seed_memory(&conn, MemorySeed {
        repo_id: REPO,
        origin: "synced",
        source_text_hash: Some("stamped-base"),
        ..MemorySeed::default()
    });
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/a.rs','src/a.rs','current',0,?1)",
        params![REPO],
    )
    .unwrap();

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(
        memory.synced_anchor_drifted,
        "the shadowed base hash must not certify a memory as current"
    );
}

#[test]
fn every_drive_by_reader_marks_including_the_one_that_hydrates_its_own_ids() {
    let conn = db();
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", Some("then"), chunk);
    // `memories_for_symbol` collects ids into a set and hydrates them itself rather than going
    // through `ids_to_memories`, so it is the reader a seam-only fix would silently miss.
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','path','src/a.rs','src/a.rs','current',0,?1)",
        params![REPO],
    )
    .unwrap();

    let hit = crate::symbol::SymbolHit {
        symbol_id: 0,
        logical_symbol_id: None,
        logical_variant_count: None,
        logical_group_reason: None,
        file_id: 0,
        path: "src/a.rs".to_string(),
        file_kind: "source".to_string(),
        language: "rust".to_string(),
        name: "a".to_string(),
        symbol_path: "src/a.rs::a".to_string(),
        qualified_name: "src/a.rs::a".to_string(),
        kind: "function".to_string(),
        start_byte: 0,
        end_byte: 0,
        signature: None,
        docs: None,
        importance: None,
    };
    let found = memories_for_symbol(&conn, &hit, 10).unwrap();
    assert_eq!(found.len(), 1, "the symbol reader still finds it");
    assert!(found[0].synced_anchor_drifted, "and marks it, like the other four readers");
}

/// Seed a file+chunk owned by a specific checkout, so the active-scope filter can be exercised.
fn seed_chunk_in(conn: &Connection, path: &str, text_hash: &str, worktree: &str) -> i64 {
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation) VALUES \
         (?1,'rust','source',?2,0,0,'',?3,?4,0)",
        params![path, format!("sha-{path}"), worktree, REPO],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
         text_hash) VALUES (?1,'code',0,10,1,5,?2)",
        params![file_id, text_hash],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn set_active_worktree(conn: &Connection, worktree: &str) {
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('worktree_id', ?1)",
        [worktree],
    )
    .unwrap();
}

/// The scoped `files` view a real connection carries, in the shape `lifecycle.rs` installs:
/// the active checkout's own rows, plus base rows for paths that checkout does not override.
/// Reads under test must see exactly what this checkout serves, shadowing included.
fn install_files_view(conn: &Connection, active_worktree: &str) {
    conn.execute_batch(&format!(
        "DROP VIEW IF EXISTS temp.files;
             CREATE TEMP VIEW temp.files AS
             SELECT * FROM main.files
              WHERE repo_id = '{REPO}' AND generation = 0 AND kind != 'deleted'
                AND worktree_id = '{active_worktree}' AND worktree_id != ''
             UNION ALL
             SELECT * FROM main.files
              WHERE repo_id = '{REPO}' AND generation = 0 AND kind != 'deleted'
                AND worktree_id = ''
                AND path NOT IN (
                    SELECT path FROM main.files
                     WHERE repo_id = '{REPO}' AND generation = 0 AND kind != 'deleted'
                       AND worktree_id = '{active_worktree}' AND worktree_id != ''
                )"
    ))
    .unwrap();
}

#[test]
fn a_sibling_checkouts_chunk_never_prices_this_checkouts_anchor() {
    let conn = db();
    // The anchor names a chunk owned by ANOTHER checkout's file row — the shape a binding
    // takes when it was resolved over there. That text is not what this checkout serves, so
    // it must not decide this checkout's verdict; the memory simply goes unpriced here.
    let theirs = seed_chunk_in(&conn, "src/a.rs", "their-text", "wt-sibling");
    set_active_worktree(&conn, "wt-active");
    install_files_view(&conn, "wt-active");
    seed_memory(&conn, "m1", "synced", Some("stamped"), theirs);

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(
        !memory.synced_anchor_drifted,
        "a sibling checkout's text is not evidence about this one"
    );
}

#[test]
fn the_shared_base_row_still_prices_an_anchor_inside_a_linked_worktree() {
    let conn = db();
    // The `worktree_id = ''` base row belongs to every checkout, so working inside a linked
    // worktree must not silence drift on files that checkout has not overridden — otherwise
    // the filter above would turn every linked worktree into a blanket exemption.
    let base = seed_chunk_in(&conn, "src/a.rs", "now", "");
    set_active_worktree(&conn, "wt-active");
    install_files_view(&conn, "wt-active");
    seed_memory(&conn, "m1", "synced", Some("then"), base);

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(memory.synced_anchor_drifted, "the shared base row is this checkout's text too");
}

#[test]
fn a_superseded_generation_row_never_prices_an_anchor() {
    let conn = db();
    // A staging row from an in-flight rebuild carries a higher generation than the live one.
    // It is not what any reader is served, so it must not answer for the anchor either.
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    conn.execute("UPDATE main.files SET generation = 7 WHERE path = 'src/a.rs'", []).unwrap();
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", Some("then"), chunk);

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(
        !memory.synced_anchor_drifted,
        "no LIVE row prices this anchor, so there is no verdict to reach"
    );
}

#[test]
fn a_deleted_files_chunk_never_prices_an_anchor() {
    let conn = db();
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    conn.execute("UPDATE main.files SET kind = 'deleted' WHERE path = 'src/a.rs'", []).unwrap();
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", Some("then"), chunk);

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(!memory.synced_anchor_drifted, "a tombstone is not evidence of drift");
}

#[test]
fn an_edge_anchor_is_priced_by_its_source_files_hash() {
    let conn = db();
    // An edge anchor is stamped from the source file's `sha256` (`edge_by_id`), so that is
    // what prices it. Without this branch a peer's edge-anchored memory would go unpriced and
    // present as current no matter how far the file holding the call had moved on.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation) VALUES \
         ('src/a.rs','rust','source','moved-on',0,0,'','',?1,0)",
        params![REPO],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO edges_data(source_file_id, to_name_id, resolution_id, edge_kind_id, \
         confidence_id) VALUES (?1, 0, 0, 0, 0)",
        params![file_id],
    )
    .unwrap();
    let edge_id = conn.last_insert_rowid();
    install_files_view(&conn, "");
    fixtures::seed_memory(&conn, MemorySeed {
        repo_id: REPO,
        origin: "synced",
        source_text_hash: Some("stamped-then"),
        ..MemorySeed::default()
    });
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, edge_id, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','edge','fp','src/a.rs',?1,'current',0,?2)",
        params![edge_id, REPO],
    )
    .unwrap();

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(memory.synced_anchor_drifted, "the edge's source file is not what was stamped");
}

#[test]
fn a_drifted_memory_carries_the_flag_on_the_wire() {
    let conn = db();
    // The augmenters and the raw MCP readers return a list and never partition it, so the
    // demotion has to survive serialization or those surfaces present a drifted memory as
    // plainly current. An undrifted memory adds no field.
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", Some("then"), chunk);

    let found = memories_for_chunk(&conn, chunk, 10).unwrap();
    let json = serde_json::to_value(&found[0]).unwrap();
    assert_eq!(
        json.get("synced_anchor_drifted").and_then(serde_json::Value::as_bool),
        Some(true),
        "a consumer that never calls split_active_stale still sees the divergence"
    );

    let conn2 = db();
    let ok = seed_chunk(&conn2, "src/a.rs", "same");
    install_files_view(&conn2, "");
    seed_memory(&conn2, "m2", "synced", Some("same"), ok);
    let clean = memories_for_chunk(&conn2, ok, 10).unwrap();
    assert!(
        serde_json::to_value(&clean[0]).unwrap().get("synced_anchor_drifted").is_none(),
        "the common case stays off the wire"
    );
}

/// The row shape `seed_node_anchors` writes: portable columns only, resolved ids left NULL.
fn seed_unresolved_binding(conn: &Connection, kind: &str, path: &str, span: Option<(i64, i64)>) {
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, start_line, \
         end_line, anchor_status, created_at_ms, repo_id) VALUES \
         ('m1',?1,'portable-id',?2,?3,?4,'unverified',0,?5)",
        params![kind, path, span.map(|s| s.0), span.map(|s| s.1), REPO],
    )
    .unwrap();
}

fn seed_bare_memory(conn: &Connection, stamp: &str) {
    fixtures::seed_memory(conn, MemorySeed {
        repo_id: REPO,
        origin: "synced",
        source_text_hash: Some(stamp),
        ..MemorySeed::default()
    });
}

#[test]
fn a_freshly_seeded_symbol_anchor_is_priced_before_validation_resolves_it() {
    let conn = db();
    // Straight out of the drain: the seeder writes portable columns only, and nothing runs the
    // validate/relocate loop for it automatically. Keying on `chunk_id` alone would leave a
    // peer's symbol-anchored memory unpriced for as long as nobody ran `memory_validate` —
    // precisely the window in which their memories first show up.
    seed_chunk(&conn, "src/a.rs", "now");
    install_files_view(&conn, "");
    seed_bare_memory(&conn, "stamped-then");
    seed_unresolved_binding(&conn, "logical_symbol", "src/a.rs", Some((2, 4)));

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(memory.synced_anchor_drifted, "the covering chunk prices an unresolved symbol");
}

#[test]
fn a_freshly_seeded_symbol_anchor_still_matching_is_not_marked() {
    let conn = db();
    // The mirror: pricing the seeded state must not manufacture drift for a peer whose text
    // this checkout genuinely still holds.
    seed_chunk(&conn, "src/a.rs", "same");
    install_files_view(&conn, "");
    seed_bare_memory(&conn, "same");
    seed_unresolved_binding(&conn, "logical_symbol", "src/a.rs", Some((2, 4)));

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(!memory.synced_anchor_drifted, "a seeded anchor on matching text is current");
}

#[test]
fn a_freshly_seeded_edge_anchor_is_priced_by_its_source_file() {
    let conn = db();
    // An edge anchor's stamp is its source file's hash, and the seeded row already carries
    // that file's path — so the portable identity prices it exactly, with no id to resolve.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation) VALUES \
         ('src/a.rs','rust','source','moved-on',0,0,'','',?1,0)",
        params![REPO],
    )
    .unwrap();
    install_files_view(&conn, "");
    seed_bare_memory(&conn, "stamped-then");
    seed_unresolved_binding(&conn, "edge", "src/a.rs", None);

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(memory.synced_anchor_drifted, "an unresolved edge is priced by its file");
}

#[test]
fn a_validated_anchor_the_overlay_shadows_falls_back_to_the_served_chunk() {
    let conn = db();
    // A binding validated in the base checkout keeps that checkout's `chunk_id`. Inside a
    // linked worktree that overrides the file, the scoped view hides that row — so the id is
    // present but unusable, and gating the fallback on `IS NULL` would leave the memory
    // unpriced while this checkout serves changed text.
    let base_chunk = seed_chunk_in(&conn, "src/a.rs", "stamped-then", "");
    seed_chunk_in(&conn, "src/a.rs", "moved-on", "wt-active");
    set_active_worktree(&conn, "wt-active");
    install_files_view(&conn, "wt-active");
    seed_bare_memory(&conn, "stamped-then");
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, start_line, \
         end_line, chunk_id, anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','logical_symbol','sym','src/a.rs',1,5,?1,'current',0,?2)",
        params![base_chunk, REPO],
    )
    .unwrap();

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(
        memory.synced_anchor_drifted,
        "a resolved id this checkout does not serve must fall back, not go silent"
    );
}

#[test]
fn a_validated_edge_anchor_the_overlay_shadows_falls_back_to_the_served_file() {
    let conn = db();
    // The edge mirror of the case above: the edge row hangs off the base file the overlay
    // shadows, so its resolved id is present but unserved here and the path fallback must
    // price it against the text this checkout does serve.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation) VALUES \
         ('src/a.rs','rust','source','stamped-then',0,0,'','',?1,0)",
        params![REPO],
    )
    .unwrap();
    let base_file = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation) VALUES \
         ('src/a.rs','rust','source','moved-on',0,0,'','wt-active',?1,0)",
        params![REPO],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges_data(source_file_id, to_name_id, resolution_id, edge_kind_id, \
         confidence_id) VALUES (?1, 0, 0, 0, 0)",
        params![base_file],
    )
    .unwrap();
    let edge_id = conn.last_insert_rowid();
    set_active_worktree(&conn, "wt-active");
    install_files_view(&conn, "wt-active");
    seed_bare_memory(&conn, "stamped-then");
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, edge_id, \
         anchor_status, created_at_ms, repo_id) VALUES \
         ('m1','edge','fp','src/a.rs',?1,'current',0,?2)",
        params![edge_id, REPO],
    )
    .unwrap();

    let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
    mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
    assert!(
        memory.synced_anchor_drifted,
        "an edge id this checkout does not serve must fall back to its path"
    );
}

#[test]
fn marking_a_list_reaches_a_memory_no_drive_by_reader_hydrated() {
    let conn = db();
    // The augmenter's lexical lane hydrates through plain `memory_by_id`. Rendering that list
    // beside path/symbol lanes would present the same memory as current or drifted depending
    // on which lane found it, so the assembled list is marked as a whole.
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", Some("then"), chunk);

    let mut lexical = vec![memory_by_id(&conn, "m1").unwrap().unwrap()];
    assert!(!lexical[0].synced_anchor_drifted, "a plain by-id hydration carries no verdict");
    mark_drive_by_drift(&conn, &mut lexical).unwrap();
    assert!(lexical[0].synced_anchor_drifted, "marking the list reaches it");
}

#[test]
fn a_by_id_read_never_marks() {
    let conn = db();
    let chunk = seed_chunk(&conn, "src/a.rs", "now");
    install_files_view(&conn, "");
    seed_memory(&conn, "m1", "synced", Some("then"), chunk);
    // `memory_by_id` backs `memory_get` and `memory_search`. Drift is a drive-by presentation
    // rule, so those surfaces must be untouched by it.
    let direct = memory_by_id(&conn, "m1").unwrap().unwrap();
    assert!(!direct.synced_anchor_drifted, "memory_get / memory_search stay unaffected");
}
