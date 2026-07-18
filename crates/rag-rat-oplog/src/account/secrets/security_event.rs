//! The local sealing-adoption audit log (`sync_security_events`, sync phase C4.3b, #607).
//!
//! When the read-time adoption cross-check ([`super::sealing::current_sealing_key`]) meets an
//! accepted [`StreamKeyWrap`](super::ops::StreamKeyWrap) naming this device that either fails to
//! unwrap (an AEAD tag failure — the primary manifestation of a substituted wrap) or unwraps to a
//! key whose `key_id` disagrees with the op's signed `key_id`, it records the evidence here.
//!
//! INVARIANT: these rows are LOCAL-only. They are never replicated, never a fold input, and the
//! adoption seam never mutates a fold verdict on their account — the shared fold stays
//! device-independent (an unwrap check is only runnable by recipients, so it can never converge).
//! The evidence is device-local, queryable by a later CLI surface.

use rusqlite::{Connection, params};

use super::super::AccountId;
use super::super::keywrap::KeyId;
use crate::stream::StreamId;

/// The closed set of sealing-adoption audit event kinds, persisted as the `kind` TEXT column via
/// [`as_db_str`](Self::as_db_str). The machine strings are stable schema — a rename is a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncSecurityEventKind {
    /// A wrap naming this device unwrapped cleanly, but the recovered key's `key_id` disagrees with
    /// the op's signed `key_id` (the residual a valid authority gate can't catch: a wrong key
    /// inside a validly-signed owner op / impl bug).
    WrapKeyIdMismatch,
    /// A wrap naming this device failed to unwrap — AEAD tag failure, blocklisted ephemeral key, or
    /// non-contributory DH. The primary manifestation of a substituted/corrupt wrap, since the
    /// key_id compare is only reachable after the tag already verified.
    WrapUnwrapFailed,
}

impl SyncSecurityEventKind {
    pub(super) fn as_db_str(self) -> &'static str {
        match self {
            Self::WrapKeyIdMismatch => "wrap_key_id_mismatch",
            Self::WrapUnwrapFailed => "wrap_unwrap_failed",
        }
    }

    pub(super) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "wrap_key_id_mismatch" => Some(Self::WrapKeyIdMismatch),
            "wrap_unwrap_failed" => Some(Self::WrapUnwrapFailed),
            _ => None,
        }
    }
}

/// One sealing-adoption audit event. The `key_epoch` is persisted as 8-byte BE (not INT) so a
/// `u64` epoch round-trips without the `i64` narrowing hazard.
pub(super) struct SyncSecurityEvent {
    pub(super) kind: SyncSecurityEventKind,
    pub(super) account_id: AccountId,
    pub(super) stream_id: StreamId,
    pub(super) key_epoch: u64,
    /// The accepted wrap op whose wrap failed the cross-check — the dedup key with `kind`.
    pub(super) entry_hash: [u8; 32],
    /// The op's signed (claimed) `key_id` — the value the recovered key was required to match.
    pub(super) expected_key_id: Option<KeyId>,
    /// The `key_id` of the key actually recovered; `None` for an unwrap failure (no key
    /// recovered).
    pub(super) observed_key_id: Option<KeyId>,
    pub(super) observed_at_ms: i64,
}

/// Append one audit event, `INSERT OR IGNORE` on the `UNIQUE(kind, entry_hash)` dedup key so a hot
/// seal-path retry never re-appends the same evidence for one op. Autocommits on `&Connection` —
/// see the durability note on [`super::sealing::current_sealing_key`].
pub(super) fn record_sync_security_event(
    conn: &Connection,
    event: &SyncSecurityEvent,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO sync_security_events(
             kind, account_id, stream_id, key_epoch, entry_hash,
             expected_key_id, observed_key_id, observed_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.kind.as_db_str(),
            event.account_id.to_bytes().as_slice(),
            event.stream_id.to_bytes().as_slice(),
            event.key_epoch.to_be_bytes().as_slice(),
            event.entry_hash.as_slice(),
            event.expected_key_id.map(|k| k.to_bytes().to_vec()),
            event.observed_key_id.map(|k| k.to_bytes().to_vec()),
            event.observed_at_ms,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;
    use rusqlite::Connection;

    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    #[test]
    fn kind_round_trips_through_the_db_string() {
        for kind in
            [SyncSecurityEventKind::WrapKeyIdMismatch, SyncSecurityEventKind::WrapUnwrapFailed]
        {
            assert_eq!(SyncSecurityEventKind::from_db_str(kind.as_db_str()), Some(kind));
        }
        assert_eq!(SyncSecurityEventKind::from_db_str("something_else"), None);
        // The persisted strings are schema — pin them.
        assert_eq!(SyncSecurityEventKind::WrapKeyIdMismatch.as_db_str(), "wrap_key_id_mismatch");
        assert_eq!(SyncSecurityEventKind::WrapUnwrapFailed.as_db_str(), "wrap_unwrap_failed");
    }

    #[test]
    fn record_is_idempotent_per_kind_and_entry_hash() {
        let conn = db();
        let event = SyncSecurityEvent {
            kind: SyncSecurityEventKind::WrapUnwrapFailed,
            account_id: AccountId::from_bytes([1; 32]),
            stream_id: StreamId::from_bytes([2; 32]),
            key_epoch: 7,
            entry_hash: [9; 32],
            expected_key_id: Some(KeyId::from_bytes([3; 32])),
            observed_key_id: None,
            observed_at_ms: 111,
        };
        record_sync_security_event(&conn, &event).unwrap();
        // A repeated (kind, entry_hash) is IGNOREd — the hot-path retry guard.
        record_sync_security_event(&conn, &event).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_security_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "the same (kind, entry_hash) does not re-append");

        // A different kind for the SAME entry is a distinct event, and carries an observed key_id.
        record_sync_security_event(&conn, &SyncSecurityEvent {
            kind: SyncSecurityEventKind::WrapKeyIdMismatch,
            observed_key_id: Some(KeyId::from_bytes([4; 32])),
            ..event
        })
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_security_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "a distinct kind for the same entry is its own event");

        // The columns round-trip; key_epoch is 8-byte BE and the null/present key_ids persist.
        let (epoch_be, expected, observed): (Vec<u8>, Vec<u8>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT key_epoch, expected_key_id, observed_key_id FROM sync_security_events
                 WHERE kind = 'wrap_key_id_mismatch'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(epoch_be, 7u64.to_be_bytes().to_vec(), "key_epoch stored as 8-byte BE");
        assert_eq!(expected, [3u8; 32].to_vec());
        assert_eq!(observed, Some([4u8; 32].to_vec()));

        let unwrap_failed_observed: Option<Vec<u8>> = conn
            .query_row(
                "SELECT observed_key_id FROM sync_security_events WHERE kind = \
                 'wrap_unwrap_failed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unwrap_failed_observed, None, "an unwrap failure records no observed key_id");
    }
}
