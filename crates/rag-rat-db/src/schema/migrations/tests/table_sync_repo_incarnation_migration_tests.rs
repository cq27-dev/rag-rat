use super::*;

#[test]
fn legacy_table_sync_state_becomes_a_retained_witness_and_incarnation_scoped_schema() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE table_sync_entries(
                 entry_hash BLOB PRIMARY KEY, stream_id BLOB NOT NULL,
                 device_fingerprint BLOB NOT NULL, lamport INTEGER NOT NULL,
                 prev_hash BLOB, signed_bytes BLOB NOT NULL, received_at_ms INTEGER NOT NULL
             );
             CREATE TABLE table_sync_gapped_entries(
                 entry_hash BLOB PRIMARY KEY, stream_id BLOB NOT NULL,
                 device_fingerprint BLOB NOT NULL, lamport INTEGER NOT NULL,
                 prev_hash BLOB NOT NULL, signed_bytes BLOB NOT NULL, gapped_at_ms INTEGER NOT NULL
             );
             CREATE TABLE table_sync_streams(
                 stream_id BLOB PRIMARY KEY, repo_id TEXT NOT NULL,
                 account_id BLOB NOT NULL, scope_id TEXT NOT NULL
             );
             CREATE TABLE sync_published_rows(
                 repo_id TEXT, table_name TEXT, row_pk TEXT, synced_hash TEXT, spec_version INTEGER
             );
             CREATE TABLE sync_row_clocks(
                 repo_id TEXT, table_name TEXT, row_pk TEXT, lamport INTEGER,
                 device_fingerprint TEXT
             );
             CREATE TABLE sync_row_tombstones(
                 repo_id TEXT, table_name TEXT, row_pk TEXT, lamport INTEGER,
                 device_fingerprint TEXT
             );
             INSERT INTO table_sync_streams VALUES(zeroblob(32), 'repo', zeroblob(32), 'demo/1');
             INSERT INTO table_sync_entries VALUES(
                 randomblob(32), zeroblob(32), zeroblob(32), 7, NULL, X'00', 0
             );",
    )
    .unwrap();

    apply_table_sync_repo_incarnations(&conn).unwrap();
    assert!(column_exists(&conn, "table_sync_streams", "incarnation_ref").unwrap());
    for table in ["sync_published_rows", "sync_row_clocks", "sync_row_tombstones"] {
        assert!(column_exists(&conn, table, "stream_id").unwrap(), "{table}");
    }
    let witnesses: i64 =
        conn.query_row("SELECT COUNT(*) FROM table_sync_chain_tips", [], |row| row.get(0)).unwrap();
    assert_eq!(witnesses, 1);
    let entries: i64 =
        conn.query_row("SELECT COUNT(*) FROM table_sync_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(entries, 0, "un-authorized /4 history is not assigned a /5 incarnation");
    assert!(!column_exists(&conn, "table_sync_chain_tips", "repo_id").unwrap());
}

#[test]
fn full_ladder_replay_repairs_a_missing_chain_tip_table() {
    let conn = Connection::open_in_memory().unwrap();
    crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    conn.execute(
        "INSERT INTO table_sync_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                 received_at_ms
             ) VALUES (?1, ?2, ?3, 3, NULL, X'00', 0)",
        rusqlite::params![[3u8; 32].as_slice(), [1u8; 32].as_slice(), [2u8; 32].as_slice()],
    )
    .unwrap();
    conn.execute_batch("DROP TABLE table_sync_chain_tips").unwrap();

    crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    let restored: (i64, Vec<u8>) = conn
        .query_row(
            "SELECT lamport, entry_hash FROM table_sync_chain_tips
                  WHERE stream_id = ?1 AND device_fingerprint = ?2",
            rusqlite::params![[1u8; 32].as_slice(), [2u8; 32].as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(restored, (3, vec![3u8; 32]));

    crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM table_sync_chain_tips", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1, "accepted-tip repair is idempotent");
}
