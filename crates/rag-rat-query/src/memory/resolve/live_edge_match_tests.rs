
use super::*;

#[test]
fn call_path_candidate_query_seeds_on_the_to_name_index() {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    let sql = format!("EXPLAIN QUERY PLAN {}", live_edge_match_sql(2));
    let mut stmt = conn.prepare(&sql).unwrap();
    let plan = stmt
        .query_map(
            rusqlite::params![
                Option::<String>::None,
                "first_target",
                "calls_name",
                Option::<String>::None,
                Option::<i64>::None,
                "source",
                "second_target",
                "calls_name",
                "qualified::target",
                Option::<i64>::None,
            ],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("idx_edges_to_name"), "query plan must use to-name index:\n{plan}");
    assert!(!plan.contains("SCAN d"), "query plan must not scan edges_data:\n{plan}");
}
