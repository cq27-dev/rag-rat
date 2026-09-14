use super::*;

#[test]
fn sync_diagnostics_share_main_and_linked_scope_without_exposing_siblings() {
    let (root, config) = git_fixture_for_overlay_tests();
    let linked = unique_temp_root();
    run_git(&root, &["worktree", "add", "-b", "diagnostic-linked", linked.to_str().unwrap()]);
    let mut linked_config = config.clone();
    linked_config.root = linked.to_path_buf();
    let db = IndexDatabase::open_config(&config).unwrap();
    let sibling_db = IndexDatabase::open_config(&linked_config).unwrap();
    assert_eq!(db.active_repo_id, sibling_db.active_repo_id);
    let conn = db.storage.connection();
    assert!(rag_rat_oplog::read_local_account(conn).unwrap().is_none());
    assert!(db.sync_row_diagnostics(10).unwrap().is_empty());
    assert!(
        rag_rat_oplog::read_local_account(conn).unwrap().is_none(),
        "a diagnostic read does not create identity"
    );
    let account = rag_rat_oplog::local_account(conn, 1).unwrap();
    for repo in [db.active_repo_id.as_str(), "unrelated-repo"] {
        conn.execute(
            "INSERT INTO \
             account_repo_incarnation_current(account_id,repository_id,incarnation_ref) VALUES \
             (?1,?2,?3)",
            rusqlite::params![account.to_bytes().as_slice(), repo, [42_u8; 32].as_slice()],
        )
        .unwrap();
    }
    let streams = rag_rat_oplog::table_sync_supported_streams(conn, account).unwrap();
    for stream in &streams {
        conn.execute(
            "INSERT INTO table_sync_row_diagnostics VALUES \
             (?1,?2,'repo_memories','r1','missing_entry')",
            rusqlite::params![stream.stream_id.as_slice(), stream.repo_id],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO table_sync_row_diagnostics VALUES \
         (?1,?2,'repo_memories','old-incarnation','missing_entry')",
        rusqlite::params![[99_u8; 32].as_slice(), db.active_repo_id],
    )
    .unwrap();
    let expected = streams.iter().filter(|s| s.repo_id == db.active_repo_id).count();
    assert!(expected > 1);
    let main = db.sync_row_diagnostics(1000).unwrap();
    assert_eq!(main.len(), expected);
    assert_eq!(main, sibling_db.sync_row_diagnostics(1000).unwrap());
    assert!(main.iter().all(|r| r.row_pk == "r1"));
    assert_eq!(db.sync_row_diagnostics(1).unwrap().len(), 1, "the cap is across streams");
    assert!(db.sync_row_diagnostics(0).unwrap().is_empty());
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM table_sync_row_diagnostics", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        streams.len() as i64 + 1,
        "reporting preserves sibling and retired-stream observations"
    );
}
