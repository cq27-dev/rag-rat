use super::*;

#[test]
fn rolls_back_when_reason_index_is_blocked() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "
            CREATE TABLE blocker(x TEXT);
            CREATE INDEX idx_memory_model_failures_reason ON blocker(x);
            ",
    )
    .unwrap();

    let err = apply_memory_model_failures_table(&conn)
        .expect_err("conflicting index name must make the migration fail");

    assert!(
        err.to_string().contains("idx_memory_model_failures_reason"),
        "the failure should come from the blocked index name, got {err}"
    );
    assert!(
        !table_exists(&conn, "memory_model_failures").unwrap(),
        "CREATE TABLE is inside the transaction and rolls back with the failed index"
    );
    let blocker_rows: i64 =
        conn.query_row("SELECT COUNT(*) FROM blocker", [], |r| r.get(0)).unwrap();
    assert_eq!(blocker_rows, 0, "the preexisting blocker table/index is left intact");
}
