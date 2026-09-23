//! A device that signed two entries at one slot of its own chain (#1417).
//!
//! The device signing key lives in the store, so a store restored from an older copy — or copied to
//! a second machine — signs under an identity whose held chains are behind what that identity has
//! already signed. Catching up is harmless: receiving its own later entries just extends the tail.
//! The harm is authoring BEFORE catching up, which signs a second entry at an already-used seq.
//! Branch selection keeps one sibling by minimum hash, and because authoring chains from the held
//! tail, the device may go on building on the loser, where everything it signs is silently forked.
//!
//! So the fork is read straight off the candidate tables rather than recorded: they are grow-only,
//! so both siblings stay held for good, and a fork the store already holds is found the same way as
//! one that arrives later. An honest device never signs two entries at one slot and nobody without
//! its key can, so two of them are proof, never a guess.
//!
//! Only the forked chain stops. The same device keeps every other chain, which is what keeps the
//! remedies reachable: another owner removes this device and the store enrolls again as a new one
//! (RFC 9750 §6.6), and a sole owner whose fork is not on the control log promotes another device
//! to owner first.

use rusqlite::{Connection, OptionalExtension, params};

use super::AccountId;
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

/// Which chain forked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkedLane {
    /// An account log (control, secrets, annex) of `account_id`.
    Account { account_id: AccountId, log_id: u8 },
    /// A content chain on `stream_id`, authored as `author_account_id`.
    Content { stream_id: StreamId, author_account_id: AccountId },
}

/// The local device holds two entries it signed at `seq` of one of its own chains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkedChain {
    pub lane: ForkedLane,
    /// The lowest slot holding two of the device's entries.
    pub seq: u64,
}

impl std::fmt::Display for ForkedChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.lane {
            ForkedLane::Account { log_id, .. } => write!(
                f,
                "ForkedChain: this device signed two entries at seq {} of its own account log \
                 {log_id}",
                self.seq
            )?,
            ForkedLane::Content { .. } => write!(
                f,
                "ForkedChain: this device signed two entries at seq {} of its own content chain",
                self.seq
            )?,
        }
        f.write_str(
            ". Its store was restored from an older copy or copied to another machine, so it \
             cannot author on that chain again: another owner must remove this device, and it \
             must enroll again as a new device (a sole owner promotes another device to owner \
             first)",
        )
    }
}

impl std::error::Error for ForkedChain {}

/// The lowest slot of `device`'s chain on `(account_id, log_id)` holding two of its entries.
pub(super) fn account_chain_fork(
    conn: &Connection,
    account_id: AccountId,
    device: DeviceFingerprint,
    log_id: u8,
) -> rusqlite::Result<Option<u64>> {
    conn.query_row(
        "SELECT seq FROM account_entries
          WHERE account_id = ?1 AND log_id = ?2 AND device_fingerprint = ?3
          GROUP BY seq HAVING COUNT(*) > 1 ORDER BY seq LIMIT 1",
        params![account_id.to_bytes().as_slice(), log_id, device.to_bytes().as_slice()],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map(|seq| seq.map(|seq| seq as u64))
}

/// The lowest slot of `device`'s content chain on `(stream_id, author_account_id)` holding two of
/// its entries. Content `seq` is a fixed-width big-endian blob, so `ORDER BY` is numeric.
pub(super) fn content_chain_fork(
    conn: &Connection,
    stream_id: StreamId,
    author_account_id: AccountId,
    device: DeviceFingerprint,
) -> anyhow::Result<Option<u64>> {
    let seq: Option<Vec<u8>> = conn
        .query_row(
            "SELECT seq FROM content_entries
              WHERE stream_id = ?1 AND author_account_id = ?2 AND device_fingerprint = ?3
              GROUP BY seq HAVING COUNT(*) > 1 ORDER BY seq LIMIT 1",
            params![
                stream_id.to_bytes().as_slice(),
                author_account_id.to_bytes().as_slice(),
                device.to_bytes().as_slice(),
            ],
            |row| row.get(0),
        )
        .optional()?;
    seq.map(|seq| Ok(u64::from_be_bytes(super::id::fixed::<8>(&seq)?))).transpose()
}

/// Every chain of the local device that holds two of its entries at one slot, for reporting. Empty
/// when the store has no device identity yet.
pub fn local_forked_chains(conn: &Connection) -> anyhow::Result<Vec<ForkedChain>> {
    let Some(device) = crate::identity::local_device_fingerprint(conn)? else {
        return Ok(Vec::new());
    };
    let device = device.to_bytes();
    let mut out = Vec::new();

    let mut account = conn.prepare(
        "SELECT account_id, log_id, MIN(seq) FROM (
             SELECT account_id, log_id, seq FROM account_entries
              WHERE device_fingerprint = ?1
              GROUP BY account_id, log_id, seq HAVING COUNT(*) > 1)
          GROUP BY account_id, log_id ORDER BY account_id, log_id",
    )?;
    let rows = account.query_map([device.as_slice()], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u8>(1)?, row.get::<_, i64>(2)?))
    })?;
    for row in rows {
        let (account_id, log_id, seq) = row?;
        out.push(ForkedChain {
            lane: ForkedLane::Account {
                account_id: AccountId::from_bytes(super::id::fixed(&account_id)?),
                log_id,
            },
            seq: seq as u64,
        });
    }

    let mut content = conn.prepare(
        "SELECT stream_id, author_account_id, MIN(seq) FROM (
             SELECT stream_id, author_account_id, seq FROM content_entries
              WHERE device_fingerprint = ?1
              GROUP BY stream_id, author_account_id, seq HAVING COUNT(*) > 1)
          GROUP BY stream_id, author_account_id ORDER BY stream_id, author_account_id",
    )?;
    let rows = content.query_map([device.as_slice()], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?))
    })?;
    for row in rows {
        let (stream_id, author_account_id, seq) = row?;
        out.push(ForkedChain {
            lane: ForkedLane::Content {
                stream_id: StreamId::from_bytes(super::id::fixed(&stream_id)?),
                author_account_id: AccountId::from_bytes(super::id::fixed(&author_account_id)?),
            },
            seq: u64::from_be_bytes(super::id::fixed::<8>(&seq)?),
        });
    }
    Ok(out)
}
