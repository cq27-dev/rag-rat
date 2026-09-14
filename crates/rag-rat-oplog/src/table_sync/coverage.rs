//! Outstanding delivery through an advertised suffix tip. These unsigned routing obligations
//! never participate in chain witnesses, Lamport allocation, or row conflict resolution.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::store::ChainCursor;
use crate::op::DeviceFingerprint;
use crate::stream::{EntryHash, StreamId};

pub(super) fn pending_tip(
    conn: &Connection,
    stream: StreamId,
    device: DeviceFingerprint,
) -> anyhow::Result<Option<ChainCursor>> {
    conn.query_row(
        "SELECT tip_lamport, tip_hash FROM table_sync_suffix_coverage
         WHERE stream_id = ?1 AND device_fingerprint = ?2",
        params![stream.to_bytes().as_slice(), device.to_bytes().as_slice()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )
    .optional()?
    .map(|(lamport, hash)| {
        Ok(ChainCursor {
            lamport: u64::try_from(lamport)?,
            entry_hash: EntryHash::try_from_sql(hash)?,
        })
    })
    .transpose()
}

pub(super) fn stream_pending(conn: &Connection, stream: StreamId) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM table_sync_suffix_coverage WHERE stream_id = ?1)",
        [stream.to_bytes().as_slice()],
        |row| row.get(0),
    )?)
}

/// Only the transaction that adopts a floor may create an obligation. Keep the first target:
/// a later peer's numerically higher tip proves no ancestry through the missing suffix.
pub(super) fn record(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
    floor: u64,
    tip: ChainCursor,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO table_sync_suffix_coverage(
            stream_id, device_fingerprint, floor_lamport, tip_lamport, tip_hash
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            stream.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            i64::try_from(floor)?,
            i64::try_from(tip.lamport)?,
            tip.entry_hash.as_slice()
        ],
    )?;
    Ok(())
}

/// Gapped, rejected and merely advertised entries cannot settle delivery. While an obligation
/// exists, ingress prohibits another discontinuous root, so exact accepted presence proves the
/// contiguous suffix arrived. This includes entries promoted behind a newly received predecessor.
pub(super) fn clear_delivered(
    tx: &Transaction<'_>,
    stream: StreamId,
    device: DeviceFingerprint,
) -> anyhow::Result<()> {
    tx.execute(
        "DELETE FROM table_sync_suffix_coverage AS c
         WHERE c.stream_id = ?1 AND c.device_fingerprint = ?2 AND EXISTS (
             SELECT 1 FROM table_sync_entries e
             WHERE e.stream_id = c.stream_id AND e.device_fingerprint = c.device_fingerprint
               AND e.lamport = c.tip_lamport AND e.entry_hash = c.tip_hash
         )",
        params![stream.to_bytes().as_slice(), device.to_bytes().as_slice()],
    )?;
    Ok(())
}
