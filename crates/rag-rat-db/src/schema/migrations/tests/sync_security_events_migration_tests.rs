use super::*;

/// Insert one adoption-audit event via the write path (`INSERT OR IGNORE`); returns rows
/// changed so a dedup can be observed as `0`.
fn insert_event(conn: &Connection, kind: &str, entry_hash: &[u8]) -> usize {
    conn.execute(
        "INSERT OR IGNORE INTO sync_security_events(
                 kind, account_id, stream_id, key_epoch, entry_hash,
                 expected_key_id, observed_key_id, observed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            kind,
            [1u8; 32].as_slice(),
            [2u8; 32].as_slice(),
            0u64.to_be_bytes().as_slice(),
            entry_hash,
            Option::<Vec<u8>>::None,
            Option::<Vec<u8>>::None,
            123i64,
        ],
    )
    .unwrap()
}

#[test]
fn table_dedupes_on_kind_and_entry_hash() {
    let conn = Connection::open_in_memory().unwrap();
    apply_sync_security_events(&conn).unwrap();
    assert!(table_exists(&conn, "sync_security_events").unwrap(), "the table is created");

    // A first event lands; a second with the SAME (kind, entry_hash) is IGNOREd (the hot
    // seal-path-retry guard); a different kind for the same entry_hash is a distinct event.
    assert_eq!(insert_event(&conn, "wrap_unwrap_failed", &[9u8; 32]), 1, "first event inserts");
    assert_eq!(
        insert_event(&conn, "wrap_unwrap_failed", &[9u8; 32]),
        0,
        "a duplicate (kind, entry_hash) is ignored, not re-appended",
    );
    assert_eq!(
        insert_event(&conn, "wrap_key_id_mismatch", &[9u8; 32]),
        1,
        "a distinct kind for the same entry is its own event",
    );
    let rows: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_security_events", [], |row| row.get(0)).unwrap();
    assert_eq!(rows, 2, "exactly the two distinct events survive");
}

#[test]
fn strict_typing_rejects_a_non_blob_account_id() {
    let conn = Connection::open_in_memory().unwrap();
    apply_sync_security_events(&conn).unwrap();
    // STRICT: `account_id` is declared BLOB, so an INTEGER literal there is a datatype mismatch
    // rather than a silently coerced value.
    let inserted = conn.execute(
        "INSERT INTO sync_security_events(
                 kind, account_id, stream_id, key_epoch, entry_hash, observed_at_ms)
             VALUES ('wrap_unwrap_failed', 5, x'02', x'0000000000000000', x'09', 1)",
        [],
    );
    assert!(inserted.is_err(), "STRICT rejects an INTEGER in the BLOB account_id column");
}
