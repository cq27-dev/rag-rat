//! Permanent external account trust survives projection rebuilds and repository purges.
use rusqlite::Connection;

pub fn apply_account_control_pins(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_control_pins(
            account_id BLOB PRIMARY KEY CHECK(length(account_id)=32),
            checkpoint_digest BLOB NOT NULL CHECK(length(checkpoint_digest)=32),
            required_version INTEGER NOT NULL CHECK(required_version=2),
            certificate BLOB NOT NULL CHECK(length(certificate)<=1024)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS account_control_pin_evidence(
            account_id BLOB NOT NULL,
            ordinal INTEGER NOT NULL,
            signed_bytes BLOB NOT NULL,
            PRIMARY KEY(account_id, ordinal)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS account_control_pin_streams(
            account_id BLOB NOT NULL, stream_id BLOB NOT NULL,
            PRIMARY KEY(account_id, stream_id)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS account_control_pin_stream_lookup ON \
         account_control_pin_streams(stream_id);
        CREATE TRIGGER IF NOT EXISTS account_control_pin_no_update BEFORE UPDATE ON \
         account_control_pins
        BEGIN SELECT RAISE(ABORT, 'account control pin is permanent'); END;
        CREATE TRIGGER IF NOT EXISTS account_control_pin_no_delete BEFORE DELETE ON \
         account_control_pins
        BEGIN SELECT RAISE(ABORT, 'account control pin is permanent'); END;
        CREATE TRIGGER IF NOT EXISTS account_control_evidence_no_update BEFORE UPDATE ON \
         account_control_pin_evidence
        BEGIN SELECT RAISE(ABORT, 'account checkpoint evidence is permanent'); END;
        CREATE TRIGGER IF NOT EXISTS account_control_evidence_no_delete BEFORE DELETE ON \
         account_control_pin_evidence
        BEGIN SELECT RAISE(ABORT, 'account checkpoint evidence is permanent'); END;",
    )
}
