use super::*;

#[test]
fn lane_triggers_are_idempotent_and_repo_scoped() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE repos(repo_id TEXT PRIMARY KEY);
             CREATE TABLE repo_meta(
                 repo_id TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT,
                 PRIMARY KEY(repo_id, key)
             );
             CREATE TABLE repo_memories(repo_id TEXT NOT NULL);
             CREATE TABLE papertrail_items(repo_id TEXT NOT NULL);
             CREATE TABLE clone_refinements(repo_id TEXT NOT NULL);
             CREATE TABLE oracle_runs(repo_id TEXT NOT NULL);
             CREATE TABLE clone_graph_generations(repo_id TEXT NOT NULL, generation INTEGER);
             INSERT INTO repos VALUES ('active'), ('sibling');",
    )
    .unwrap();

    apply_lens_lane_revisions(&conn).unwrap();
    apply_lens_lane_revisions(&conn).expect("trigger replay is a no-op");
    conn.execute("INSERT INTO repo_memories VALUES ('active')", []).unwrap();
    conn.execute("INSERT INTO papertrail_items VALUES ('sibling')", []).unwrap();
    conn.execute("INSERT INTO oracle_runs VALUES ('active')", []).unwrap();
    conn.execute("INSERT INTO repo_meta VALUES ('active', 'clone_graph_live_generation', '7')", [])
        .unwrap();
    conn.execute("INSERT INTO clone_graph_generations VALUES ('active', 7)", []).unwrap();

    let revision = |repo_id, key| {
        crate::meta::repo_meta(&conn, repo_id, key)
            .unwrap()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0)
    };
    assert_eq!(revision("active", crate::meta::LENS_MEMORIES_REVISION_META), 1);
    assert_eq!(revision("sibling", crate::meta::LENS_MEMORIES_REVISION_META), 0);
    assert_eq!(revision("sibling", crate::meta::LENS_PAPERTRAIL_REVISION_META), 1);
    assert_eq!(revision("active", crate::meta::LENS_PAPERTRAIL_REVISION_META), 0);
    assert_eq!(revision("active", crate::meta::LENS_SYMBOLS_REVISION_META), 1);
    assert_eq!(revision("active", crate::meta::LENS_CLONES_REVISION_META), 2);
}
