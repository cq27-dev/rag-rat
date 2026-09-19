//! Permanent external account trust survives projection rebuilds and repository purges.
use rusqlite::Connection;

use crate::schema::migrations::add_column_if_missing;

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

/// V131 (#1367): record the pre-cut view a control-v2 cut names, so candidate admission can tell a
/// manifest some stored cut cites from one nothing cites.
///
/// The reserve above the ordinary budget exists to keep a cut's evidence admissible. Without a
/// column to ask the question, every entry wearing the manifest tag reaches that reserve, and
/// filling it with manifests no cut names leaves an honest cut's evidence permanently unadmitted —
/// the terminal state the reserve exists to prevent.
///
/// Nullable and additive: an entry naming no view — every v1 entry, every annex entry — leaves it
/// NULL, and the partial index carries only the rows that name one. `BLOB` is a valid STRICT type.
/// Idempotent via `add_column_if_missing`, so a torn replay reconverges.
///
/// Rows stored BEFORE this migration are backfilled through the hook, not left NULL. The digest
/// lives inside a signed payload this crate sits below and cannot decode, so the decode runs in the
/// oplog layer. Leaving them NULL would not be the bounded cost it first appears: a manifest whose
/// only citation is a pre-upgrade cut would be charged the ordinary budget, and the reserve exists
/// precisely for the case where that budget is exhausted — so those rows would keep the very defect
/// this migration fixes, permanently, because candidate history is grow-only.
pub fn apply_account_view_citations(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "account_entries", "cited_view_digest", "BLOB")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS account_entry_view_citations
             ON account_entries(account_id, cited_view_digest)
             WHERE cited_view_digest IS NOT NULL;",
    )?;
    (hooks.backfill_cited_view_digests)(conn)
}
