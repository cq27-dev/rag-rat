//! Local row observations. They neither authorize a write nor pin replication history.
//! One current cause per row replaces repeated warnings; successful settlement clears it.

use std::collections::BTreeSet;

use rusqlite::{Connection, Transaction, params};

use super::apply::{self, RowKey};
use super::engine::SyncCtx;
use super::row_op;
use super::scope_stream::scope_stream_id;
use crate::stream::StreamId;

/// Stable local diagnostic tokens. Unknown persisted tokens remain visible in query results.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum TableSyncRowCause {
    MissingClock,
    InvalidClockDevice,
    MissingEntry,
    UndecodableEntry,
    UnknownOperation,
    WrongOperation,
    WrongTable,
    WrongKey,
    UnprojectableWinner,
    UnreadableRow,
    SelfApplySuperseded,
}

impl TableSyncRowCause {
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }
    pub fn from_db_str(value: &str) -> Option<Self> {
        value.parse().ok()
    }
}

/// Last observed unresolved state; a raw local edit is reflected on the next scan or replay.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TableSyncRowDiagnostic {
    pub stream_id: String,
    pub table_name: String,
    /// Canonical typed primary-key representation used by the merge bookkeeping.
    pub row_pk: String,
    /// Raw stable token, preserving observations written by newer binaries.
    pub cause: String,
    /// A failed self-apply remains unresolved until a successful publication or deletion.
    pub self_apply_failed: bool,
}

/// One stream/repository, ordered by `(table_name, row_pk)`. Continue after the last returned row.
pub struct TableSyncDiagnosticQuery<'a> {
    pub stream_id: [u8; 32],
    pub repo_id: &'a str,
    pub after: Option<(&'a str, &'a str)>,
    /// Clamped to 1000; zero returns no rows.
    pub limit: usize,
}

pub fn table_sync_row_diagnostics(
    conn: &Connection,
    query: &TableSyncDiagnosticQuery<'_>,
) -> anyhow::Result<Vec<TableSyncRowDiagnostic>> {
    let mut stmt = conn.prepare(
        "SELECT table_name, row_pk, cause, self_apply_failed FROM table_sync_row_diagnostics
         WHERE stream_id = ?1 AND repo_id = ?2
           AND (?3 IS NULL OR (table_name, row_pk) > (?3, ?4))
         ORDER BY table_name, row_pk LIMIT ?5",
    )?;
    let rows = stmt.query_map(
        params![
            query.stream_id.as_slice(),
            query.repo_id,
            query.after.map(|p| p.0),
            query.after.map(|p| p.1),
            query.limit.min(1000) as i64,
        ],
        |r| {
            Ok(TableSyncRowDiagnostic {
                stream_id: rag_rat_base::hash::hex_lower(&query.stream_id),
                table_name: r.get(0)?,
                row_pk: r.get(1)?,
                cause: r.get(2)?,
                self_apply_failed: r.get(3)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn record(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
    cause: TableSyncRowCause,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO table_sync_row_diagnostics(stream_id, repo_id, table_name, row_pk, cause)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(stream_id, repo_id, table_name, row_pk) DO UPDATE SET cause = excluded.cause
         WHERE cause != excluded.cause",
        params![
            key.stream.to_bytes().as_slice(),
            key.repo_id,
            key.table,
            key.row_pk,
            cause.as_db_str()
        ],
    )?;
    Ok(())
}

pub(super) fn clear(tx: &Transaction<'_>, key: &RowKey<'_>) -> anyhow::Result<()> {
    tx.execute(
        "DELETE FROM table_sync_row_diagnostics
        WHERE stream_id = ?1 AND repo_id = ?2 AND table_name = ?3 AND row_pk = ?4",
        params![key.stream.to_bytes().as_slice(), key.repo_id, key.table, key.row_pk],
    )?;
    Ok(())
}

pub(super) fn clear_observed(
    tx: &Transaction<'_>,
    key: &RowKey<'_>,
    winner_resolved: bool,
) -> anyhow::Result<()> {
    let stream_bytes = key.stream.to_bytes();
    let args =
        params![stream_bytes.as_slice(), key.repo_id, key.table, key.row_pk, winner_resolved];
    tx.execute(
        "UPDATE table_sync_row_diagnostics SET cause = 'self_apply_superseded'
        WHERE stream_id = ?1 AND repo_id = ?2 AND table_name = ?3 AND row_pk = ?4
          AND self_apply_failed = 1 AND (?5 OR cause = 'unreadable_row') AND cause != \
         'self_apply_superseded'",
        args,
    )?;
    tx.execute(
        "DELETE FROM table_sync_row_diagnostics
        WHERE stream_id = ?1 AND repo_id = ?2 AND table_name = ?3 AND row_pk = ?4
          AND self_apply_failed = 0 AND (?5 OR cause = 'unreadable_row')",
        args,
    )?;
    Ok(())
}

pub(super) fn clear_absent(
    tx: &Transaction<'_>,
    stream: StreamId,
    repo: &str,
    table: &str,
    live: &BTreeSet<String>,
) -> anyhow::Result<()> {
    let mut stmt = tx.prepare(
        "SELECT row_pk FROM table_sync_row_diagnostics WHERE stream_id = ?1 AND repo_id = ?2 AND \
         table_name = ?3",
    )?;
    let keys = stmt
        .query_map(params![stream.to_bytes().as_slice(), repo, table], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row_pk in keys {
        if !live.contains(&row_pk) {
            let key = RowKey { stream, repo_id: repo, table, row_pk: &row_pk };
            if apply::published_hash_on_stream(tx, &key)?.is_none() {
                clear(tx, &key)?;
            } else {
                clear_observed(tx, &key, false)?;
            }
        }
    }
    Ok(())
}

/// Re-observe committed rows AFTER the failed author transaction rolled back. Never copy a
/// diagnostic from partially self-applied state, or commit the signed entry merely to keep its
/// explanation. This extra scan happens only on the error path.
pub(super) fn refresh_after_rollback(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
) -> anyhow::Result<()> {
    for spec in ctx.registry {
        let stream =
            scope_stream_id(ctx.repo_id, ctx.account_id, ctx.incarnation_ref, spec.scope_id);
        let mut live = BTreeSet::new();
        for row in apply::read_all_rows(tx, spec, ctx.repo_id)? {
            let (pk, cells) = match row {
                apply::ScannedRow::Readable { pk, cells } => (pk, Some(cells)),
                apply::ScannedRow::Unpublishable { pk } => (pk, None),
                apply::ScannedRow::Unaddressable => continue,
            };
            let row_pk = row_op::row_pk_string(&pk);
            live.insert(row_pk.clone());
            let key = RowKey { stream, repo_id: ctx.repo_id, table: spec.name, row_pk: &row_pk };
            match cells {
                None => record(tx, &key, TableSyncRowCause::UnreadableRow)?,
                Some(cells) if apply::published_hash_on_stream(tx, &key)?.is_some() => {
                    // Also inspect current-spec clocks: a corrupt clock can prevent self-apply
                    // even when the anti-echo hash itself is comparable.
                    let _ =
                        apply::stale_row_disposition(tx, spec, ctx.repo_id, stream, &pk, &cells)?;
                },
                Some(_) => clear_observed(tx, &key, false)?,
            }
        }
        clear_absent(tx, stream, ctx.repo_id, spec.name, &live)?;
    }
    Ok(())
}

/// A self-apply failure needs its row identity after the author transaction is gone.
#[derive(Debug)]
pub(super) struct SelfApplyConflict {
    pub stream: StreamId,
    pub repo_id: String,
    pub table: String,
    pub row_pk: String,
}

impl std::fmt::Display for SelfApplyConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "table-sync: a locally-produced op lost its own self-apply on `{}` — its row clock or \
             tombstone outranks local authoring",
            self.table
        )
    }
}
impl std::error::Error for SelfApplyConflict {}

impl SelfApplyConflict {
    pub(super) fn record_if_unexplained(&self, tx: &Transaction<'_>) -> anyhow::Result<()> {
        // Keep a more specific winner-resolution cause from the rolled-back view if available.
        tx.execute(
            "INSERT INTO table_sync_row_diagnostics(stream_id, repo_id, table_name, row_pk, \
             cause, self_apply_failed)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)
             ON CONFLICT(stream_id, repo_id, table_name, row_pk) DO UPDATE SET self_apply_failed = \
             1",
            params![
                self.stream.to_bytes().as_slice(),
                self.repo_id,
                self.table,
                self.row_pk,
                TableSyncRowCause::SelfApplySuperseded.as_db_str()
            ],
        )?;
        Ok(())
    }
}
