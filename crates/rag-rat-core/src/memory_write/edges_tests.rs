use rag_rat_oplog::StreamId;
use rag_rat_query::memory::edge_key;
use rusqlite::{Connection, params};

use super::unauthored_edges;

const REPO: &str = "repo-a";

/// A DB with the memory schema, one registered repo, and the connection scoped to it — the
/// minimal setup `memory_repo_scope` needs to resolve an active repo. Mirrors
/// `authoring::tests::scoped_conn` (each module's test scaffolding is self-contained).
fn scoped_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
        [REPO],
    )
    .unwrap();
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
fn insert_memory(conn: &Connection, id: &str, status: &str, created_at_ms: i64) {
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?1, 'body', 'high', ?2, 'agent', ?3, ?3, 'agent', 'h', 'v1',
                 ?4)",
        params![id, status, created_at_ms, REPO],
    )
    .unwrap();
}
/// Insert a node-edge by RAW SQL, bypassing the wired `add_edge` author — a "ghost edge" that
/// exists in `repo_node_edges` but was never signed into the op-log. Returns the computed
/// `edge_key` so callers can seed/inspect the projection by it.
fn insert_raw_node_edge(conn: &Connection, source: &str, relation: &str, target: &str) -> String {
    let key = edge_key(source, relation, "node", target);
    conn.execute(
        "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation,
                 target_repo_id, target_kind, target_anchor, target_node_id, anchor_status,
                 created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?2, 'node', ?5, ?5, 'current', 100)",
        params![key, REPO, source, relation, target],
    )
    .unwrap();
    key
}

/// #541/#664: the reconcile's edge reader anti-joins `repo_node_edges` against the accepted-`/3`
/// projection `content_projected_edges` and re-resolves what it returns. Proves BOTH halves of
/// the correctness crux: (a) only the edge absent from the projection comes back, and (b)
/// its `target_repo_id` — deliberately stale on the stored row, simulating an add-time
/// snapshot left behind by a repo-id re-point — is repaired to the CURRENT owner before it
/// would be signed.
#[test]
fn unauthored_edges_returns_only_edges_absent_from_the_projection_reresolved() {
    let conn = scoped_conn();
    insert_memory(&conn, "mem_a", "active", 100);
    insert_memory(&conn, "mem_b", "active", 200);
    insert_memory(&conn, "mem_c", "active", 300);
    let authored_key = insert_raw_node_edge(&conn, "mem_a", "relates_to", "mem_b");
    let ghost_key = insert_raw_node_edge(&conn, "mem_a", "depends_on", "mem_c");

    // `stream` is an opaque `StreamId` here — the anti-join only needs seed/query agreement.
    let stream = StreamId::from_bytes([0x11; 32]);
    // Seed the accepted-`/3` projection with the `relates_to` edge only — it is already
    // authored.
    conn.execute(
        "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json)
             VALUES (?1, ?2, '{}', NULL)",
        params![stream.to_bytes().as_slice(), authored_key],
    )
    .unwrap();
    // Simulate an add-time snapshot gone stale: the ghost edge's stored `target_repo_id` no
    // longer matches mem_c's CURRENT owning repo (as if a repo-id re-point happened after the
    // edge row was written).
    conn.execute(
        "UPDATE repo_node_edges SET target_repo_id = 'stale-repo-id' WHERE edge_key = ?1",
        [&ghost_key],
    )
    .unwrap();

    let missing = unauthored_edges(&conn, REPO, stream).unwrap();
    assert_eq!(
        missing.iter().map(|e| e.edge_key.as_str()).collect::<Vec<_>>(),
        [ghost_key.as_str()],
        "only the edge absent from the projection returns"
    );
    assert_eq!(
        missing[0].target_repo_id, REPO,
        "reresolve_on_read must repair the stale stored target_repo_id to the CURRENT owner \
         before the reconcile signs it"
    );
}

/// A TOMBSTONED edge (retained in the projection with `present = 0`) is NOT returned as
/// unauthored — so a foreign `EdgeRemove` is honored, not re-authored at a fresh Lamport (the
/// edge-resurrection growth loop, #691 A-pre). Before tombstones were retained, the removed
/// edge was absent from the projection and came back here.
#[test]
fn a_tombstoned_edge_is_not_re_authored() {
    let conn = scoped_conn();
    insert_memory(&conn, "mem_a", "active", 100);
    insert_memory(&conn, "mem_b", "active", 200);
    let key = insert_raw_node_edge(&conn, "mem_a", "relates_to", "mem_b");
    let stream = StreamId::from_bytes([0x11; 32]);
    conn.execute(
        "INSERT INTO content_projected_edges(
                 stream_id, edge_key, spec_json, resolved_json, present)
             VALUES (?1, ?2, '{}', NULL, 0)",
        params![stream.to_bytes().as_slice(), key],
    )
    .unwrap();
    assert!(
        unauthored_edges(&conn, REPO, stream).unwrap().is_empty(),
        "a tombstoned edge is honored, never re-authored",
    );
}

/// A SYNCED edge is never the reconcile's to author — even if its projection row is absent (its
/// acceptance was revoked) — or the local device would forge authorship of removed content
/// (#691 A-pre, Trace 2). A local edge in the same position WOULD be re-authored.
#[test]
fn a_synced_edge_is_never_re_authored() {
    let conn = scoped_conn();
    insert_memory(&conn, "mem_a", "active", 100);
    insert_memory(&conn, "mem_b", "active", 200);
    let key = insert_raw_node_edge(&conn, "mem_a", "relates_to", "mem_b");
    conn.execute("UPDATE repo_node_edges SET origin = 'synced' WHERE edge_key = ?1", [&key])
        .unwrap();
    let stream = StreamId::from_bytes([0x22; 32]);
    assert!(
        unauthored_edges(&conn, REPO, stream).unwrap().is_empty(),
        "a synced edge is not re-authored even when absent from the projection",
    );
}
