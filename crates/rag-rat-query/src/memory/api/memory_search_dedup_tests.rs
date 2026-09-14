
use super::*;

/// A corpus where one memory's FTS mirror carries a stray SECOND row — what an interrupted
/// heal, or an import that inserts before its scoped DELETE, leaves behind. The bodies differ,
/// so the two rows score differently and a `DISTINCT` keyed on the id + score pair keeps both.
fn conn_with_duplicated_fts_row() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
        [],
    )
    .unwrap();
    let insert_memory = |id: &str, body: &str| {
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status,
                        created_at_ms, updated_at_ms, source, memory_version, repo_id)
                 VALUES (?1, 'Invariant', 'Quokkaform routing', ?2, 'high', 'active', 0, 0,
                         'agent', 'v1', 'r')",
            [id, body],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
                 VALUES ('r', ?1, 'Quokkaform routing', ?2, 'Invariant', '')",
            [id, body],
        )
        .unwrap();
    };
    // `m` is the strongest match by term frequency, so BOTH of its rows outrank the others.
    insert_memory("m", "quokkaform quokkaform quokkaform");
    for other in ["m2", "m3", "m4"] {
        insert_memory(other, "quokkaform is pinned by the router on every rebuild");
    }
    // The stray duplicate mirror row for `m`, scoring differently from its real one.
    conn.execute(
        "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
             VALUES ('r', 'm', 'Quokkaform routing', 'quokkaform quokkaform quokkaform rebuild',
                     'Invariant', '')",
        [],
    )
    .unwrap();
    conn
}

#[test]
fn duplicate_fts_rows_collapse_to_one_hit_per_memory() {
    let conn = conn_with_duplicated_fts_row();
    let hits = memory_search(&conn, "quokkaform", 10).unwrap();
    assert_eq!(hits.len(), 4, "one hit per memory, not one per FTS row: {hits:?}");
}

/// The duplicate must not eat a result slot: `limit` counts distinct memories, so it has to be
/// applied AFTER the duplicate rows collapse, not to the raw FTS row set.
///
/// The DISTINCTNESS of the ids is the whole claim — a row count of 3 alone is exactly what the
/// rejected `DISTINCT (memory_id, bm25)` shape returns, with `m` twice and one memory pushed
/// out.
#[test]
fn a_duplicate_fts_row_does_not_consume_a_limit_slot() {
    let conn = conn_with_duplicated_fts_row();
    let hits = memory_search(&conn, "quokkaform", 3).unwrap();
    let ids: BTreeSet<&str> = hits.iter().map(|hit| hit.memory_id.as_str()).collect();
    assert_eq!(hits.len(), 3, "the limit is spent in full: {hits:?}");
    assert_eq!(ids.len(), 3, "limit counts distinct memories, not FTS rows: {hits:?}");
}
