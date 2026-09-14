use super::*;
use crate::memory::fixtures::{self, MemorySeed};

fn remap_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                    commit_sha, worktree_id, repo_id, generation)
             VALUES ('src/lib.rs', 'rust', 'source', 'sha', 0, 0, '', '', 'r', 0)",
        [],
    )
    .unwrap();
    fixtures::seed_memory(&conn, MemorySeed {
        id: "m",
        created_by: None,
        created_at_ms: 0,
        updated_at_ms: 0,
        ..MemorySeed::default()
    });
    conn
}

fn seed_callee(conn: &Connection, logical_id: i64, name: &str) -> i64 {
    let qualified_name = format!("src/lib.rs::{name}");
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", [&qualified_name])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, scope_path, kind,
                    start_byte, end_byte, start_line, end_line)
             VALUES (1, 'rust', ?1, (SELECT id FROM name_strings WHERE value = ?2), ?1,
                     'function', 0, 1, 1, 1)",
        params![name, qualified_name],
    )
    .unwrap();
    let symbol_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind,
                    variant_count, group_reason, repo_id)
             VALUES (?1, 'rust', 'src/lib.rs', ?2,
                     (SELECT id FROM name_strings WHERE value = ?3), 'function', 1, 'exact', 'r')",
        params![logical_id, name, qualified_name],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, end_line)
             VALUES (?1, ?2, 1, 1)",
        params![logical_id, symbol_id],
    )
    .unwrap();
    symbol_id
}

fn seed_edge(conn: &Connection, target_symbol_id: i64) -> CallPathEdge {
    conn.execute(
        "INSERT INTO edges(from_name, to_name, edge_kind, confidence, receiver_hint,
                    receiver_type_hint, source_file_id, source_start_line, source_end_line,
                    to_symbol_id)
             VALUES ('caller', 'run', 'calls_name', 'exact', 'recv', 'Worker', 1, 10, 10, ?1)",
        [target_symbol_id],
    )
    .unwrap();
    let edge_id: i64 =
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap();
    call_path_edge_by_id(conn, edge_id).unwrap().unwrap()
}

fn seed_path(
    conn: &Connection,
    edge: &CallPathEdge,
    summary: &str,
    endpoint: Option<i64>,
    created_at_ms: i64,
) -> String {
    let hash = compute_edge_sequence_hash([edge.fingerprint.as_str()]);
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id,
                    logical_symbol_id, anchor_status, created_at_ms, repo_id)
             VALUES ('m', 'call_path', ?1, ?2, 'current', ?3, 'r')",
        params![hash, endpoint, created_at_ms],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, start_logical_symbol_id,
                    edge_sequence_hash, path_summary, created_at_ms)
             VALUES ('m', ?1, ?2, ?3, ?4)",
        params![endpoint, hash, summary, created_at_ms],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal,
                    edge_fingerprint, from_name, to_name, edge_kind, receiver_hint,
                    callee_logical_symbol_id, callee_identity_known)
             VALUES ('m', ?1, 0, ?2, ?3, ?4, ?5, ?6, ?7, 1)",
        params![
            hash,
            edge.fingerprint,
            edge.from_name,
            edge.to_name,
            edge.edge_kind,
            edge.receiver_hint,
            edge.callee_logical_symbol_id
        ],
    )
    .unwrap();
    hash
}

#[test]
fn call_path_hash_swaps_stage_every_key_before_finalization() {
    let conn = remap_db();
    let alpha = seed_callee(&conn, 11, "Alpha");
    let beta = seed_callee(&conn, 22, "Beta");
    let alpha_edge = seed_edge(&conn, alpha);
    let beta_edge = seed_edge(&conn, beta);
    let alpha_hash = seed_path(&conn, &alpha_edge, "alpha path", Some(11), 2);
    let beta_hash = seed_path(&conn, &beta_edge, "beta path", Some(22), 3);

    remap_call_path_callee_logical_symbol_ids(&conn, &conn, &[(11, Some(22)), (22, Some(11))])
        .unwrap();

    let alpha_summary: String = conn
        .query_row(
            "SELECT path_summary FROM repo_memory_call_paths
                  WHERE memory_id = 'm' AND edge_sequence_hash = ?1",
            [&beta_hash],
            |row| row.get(0),
        )
        .unwrap();
    let beta_summary: String = conn
        .query_row(
            "SELECT path_summary FROM repo_memory_call_paths
                  WHERE memory_id = 'm' AND edge_sequence_hash = ?1",
            [&alpha_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(alpha_summary, "alpha path");
    assert_eq!(beta_summary, "beta path");
    for table in ["repo_memory_bindings", "repo_memory_call_paths", "repo_memory_call_path_edges"] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE memory_id = 'm'"), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 2, "both swapped rows survive in {table}");
    }
}

#[test]
fn many_to_one_call_paths_converge_onto_an_existing_destination() {
    let conn = remap_db();
    let alpha = seed_callee(&conn, 11, "Alpha");
    let beta = seed_callee(&conn, 22, "Beta");
    let target = seed_callee(&conn, 33, "Target");
    let alpha_edge = seed_edge(&conn, alpha);
    let beta_edge = seed_edge(&conn, beta);
    let target_edge = seed_edge(&conn, target);
    seed_path(&conn, &alpha_edge, "alpha path", Some(11), 2);
    seed_path(&conn, &beta_edge, "beta path", Some(22), 3);
    let target_hash = seed_path(&conn, &target_edge, "existing target", Some(33), 4);

    remap_call_path_callee_logical_symbol_ids(&conn, &conn, &[(11, Some(33)), (22, Some(33))])
        .unwrap();

    let parent: (i64, String, Option<i64>) = conn
        .query_row(
            "SELECT COUNT(*), path_summary, start_logical_symbol_id
                   FROM repo_memory_call_paths WHERE memory_id = 'm' AND edge_sequence_hash = ?1",
            [&target_hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(parent, (1, "existing target".to_string(), None));
    let binding: (i64, Option<i64>) = conn
        .query_row(
            "SELECT COUNT(*), logical_symbol_id FROM repo_memory_bindings
                  WHERE memory_id = 'm' AND binding_kind = 'call_path' AND binding_id = ?1",
            [&target_hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(binding, (1, None));
    let edge_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_call_path_edges
                  WHERE memory_id = 'm' AND edge_sequence_hash = ?1",
            [&target_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(edge_count, 1, "the existing valid edge sequence wins deterministically");
}

#[test]
fn missing_remap_evidence_cannot_revive_through_the_old_fingerprint() {
    let conn = remap_db();
    let alpha = seed_callee(&conn, 11, "Alpha");
    let edge = seed_edge(&conn, alpha);
    let old_fingerprint = edge.fingerprint.clone();
    seed_path(&conn, &edge, "missing edge", Some(11), 0);
    conn.execute("DELETE FROM edges_data", []).unwrap();

    remap_call_path_callee_logical_symbol_ids(&conn, &conn, &[(11, None)]).unwrap();
    let stored: (String, Option<i64>, i64) = conn
        .query_row(
            "SELECT edge_fingerprint, callee_logical_symbol_id, callee_identity_known
                   FROM repo_memory_call_path_edges WHERE memory_id = 'm'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(stored.0.starts_with("invalidated-call-path-remap:"));
    assert_ne!(stored.0, old_fingerprint);
    assert_eq!((stored.1, stored.2), (None, 1));

    let replacement = seed_edge(&conn, alpha);
    assert_eq!(replacement.fingerprint, old_fingerprint);
    assert!(edge_by_fingerprint(&conn, &old_fingerprint).unwrap().is_some());
    assert!(
        edge_by_fingerprint(&conn, &stored.0).unwrap().is_none(),
        "the invalidated persisted identity cannot match the replacement edge"
    );
}
