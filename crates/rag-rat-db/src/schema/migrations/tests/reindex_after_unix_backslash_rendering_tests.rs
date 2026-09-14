use super::*;

/// A `repo_meta` + `parser_failures` pair minimal enough to drive the V098 body, seeded with
/// every marker the migration clears plus one control row it must leave untouched.
fn seed(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE repo_meta(
                 repo_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                 PRIMARY KEY(repo_id, key));
             CREATE TABLE parser_failures(
                 repo_id TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(repo_id, path));",
    )
    .unwrap();
    let put = |key: &str, value: &str| {
        conn.execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('r', ?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    };
    put(crate::meta::BASE_SCOPE_DISCOVERED_META, "1");
    put(crate::meta::GIT_HISTORY_INDEXED_ROOT_META, "/some/root");
    put(&format!("{}abc123", crate::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX), "base\nlinked\n42");
    // A control the migration must NOT touch: source_root is a path-valued key too, but it is a
    // config record the next index resets from config, not a walk gate — deleting it would
    // break reads until that pass rather than merely forcing one.
    put("source_root", "/some/root");
    conn.execute("INSERT INTO parser_failures(repo_id, path) VALUES ('r', 'foo/bar.rs')", [])
        .unwrap();
}

fn has_key(conn: &Connection, key: &str) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM repo_meta WHERE key = ?1)", [key], |r| r.get(0))
        .unwrap()
}

#[test]
fn clears_every_freshness_marker_and_leaves_the_control_row() {
    let conn = Connection::open_in_memory().unwrap();
    seed(&conn);

    apply_reindex_after_unix_backslash_rendering(&conn).unwrap();

    assert!(
        !has_key(&conn, crate::meta::BASE_SCOPE_DISCOVERED_META),
        "base-scope marker gone → the next pass re-walks the tree",
    );
    assert!(
        !has_key(&conn, crate::meta::GIT_HISTORY_INDEXED_ROOT_META),
        "history root cursor gone → the next pass full-revwalks and re-reads file changes",
    );
    assert!(
        !has_key(&conn, &format!("{}abc123", crate::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX)),
        "overlay basis gone → each linked checkout re-derives its overlay",
    );
    let failures: i64 =
        conn.query_row("SELECT COUNT(*) FROM parser_failures", [], |r| r.get(0)).unwrap();
    assert_eq!(failures, 0, "the standalone parser-failure row is cleared for re-derivation");
    assert!(
        has_key(&conn, "source_root"),
        "a path record the next index resets from config, not a walk gate, is left intact",
    );
}
