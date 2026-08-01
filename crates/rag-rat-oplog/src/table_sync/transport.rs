//! Production transport seams for the `/5` table-sync engine.
//!
//! The transport sees only current repo-scoped streams and accepted history. Gapped rows remain a
//! local chain-repair detail and are never advertised or transferred.

use std::collections::BTreeSet;

use anyhow::Context;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::engine::{self, IngestOutcome, SyncCtx};
use super::registry::{SYNCABLE_TABLES, TableSpec};
use super::scope_stream::scope_stream_id;
use crate::AccountId;
use crate::account::{self, RepoIncarnationState};
use crate::device::DevicePublic;

/// One locally-supported repo-scoped table stream.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableSyncStream {
    pub repo_id: String,
    pub incarnation_ref: [u8; 32],
    pub scope_id: String,
    pub stream_id: [u8; 32],
}

/// Whether one untrusted table entry added durable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSyncIngestOutcome {
    Stored,
    NoChange,
}

/// Current repo-scoped streams supported by the production registry.
pub fn table_sync_supported_streams(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<TableSyncStream>> {
    supported_streams_against(conn, account_id, SYNCABLE_TABLES)
}

/// Recompute an advertised route from local current-incarnation authority and the production
/// registry. The advertised stream id is routing advice only; it never creates authority.
pub fn table_sync_validate_stream(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
) -> anyhow::Result<bool> {
    validate_stream_against(conn, account_id, stream, SYNCABLE_TABLES)
}

/// Accepted signed history for one validated current stream. This reads only
/// `table_sync_entries`; `table_sync_gapped_entries` is deliberately absent.
pub fn table_sync_entries_for_stream(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
) -> anyhow::Result<Vec<Vec<u8>>> {
    if !table_sync_validate_stream(conn, account_id, stream)? {
        return Ok(Vec::new());
    }
    accepted_entries(conn, stream.stream_id)
}

/// Feed one untrusted signed envelope through the existing table-sync authority, chain and payload
/// gates. Invalid/stale routes are skipped before a transaction can write any table-sync state.
pub fn table_sync_ingest(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    signed_bytes: &[u8],
    now_ms: i64,
) -> anyhow::Result<TableSyncIngestOutcome> {
    ingest_against(conn, account_id, stream, signed_bytes, now_ms, SYNCABLE_TABLES)
}

/// Inventory key for an exact signed envelope.
pub fn table_sync_signed_hash(signed_bytes: &[u8]) -> [u8; 32] {
    crate::cbor::sha256(signed_bytes)
}

fn repo_registry(registry: &[TableSpec]) -> Vec<TableSpec> {
    registry.iter().copied().filter(|spec| spec.repo_column.is_some()).collect()
}

fn supported_streams_against(
    conn: &Connection,
    account_id: AccountId,
    registry: &[TableSpec],
) -> anyhow::Result<Vec<TableSyncStream>> {
    let scopes: BTreeSet<&str> = registry
        .iter()
        .filter(|spec| spec.repo_column.is_some())
        .map(|spec| spec.scope_id)
        .collect();
    if scopes.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT repository_id, incarnation_ref
           FROM account_repo_incarnation_current
          WHERE account_id = ?1 AND incarnation_ref IS NOT NULL
          ORDER BY repository_id",
    )?;
    let rows = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut streams = Vec::with_capacity(rows.len().saturating_mul(scopes.len()));
    for (repo_id, incarnation) in rows {
        let incarnation_ref: [u8; 32] = incarnation.try_into().map_err(|got: Vec<u8>| {
            anyhow::anyhow!("stored repository incarnation must be 32 bytes, got {}", got.len())
        })?;
        for scope_id in &scopes {
            streams.push(TableSyncStream {
                stream_id: scope_stream_id(&repo_id, account_id, incarnation_ref, scope_id)
                    .to_bytes(),
                repo_id: repo_id.clone(),
                incarnation_ref,
                scope_id: (*scope_id).to_string(),
            });
        }
    }
    Ok(streams)
}

fn validate_stream_against(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    registry: &[TableSpec],
) -> anyhow::Result<bool> {
    if !registry.iter().any(|spec| spec.repo_column.is_some() && spec.scope_id == stream.scope_id) {
        return Ok(false);
    }
    let current = account::repo_incarnation_state(conn, account_id, &stream.repo_id)?;
    if current != RepoIncarnationState::Current(stream.incarnation_ref) {
        return Ok(false);
    }
    Ok(scope_stream_id(&stream.repo_id, account_id, stream.incarnation_ref, &stream.scope_id)
        .to_bytes()
        == stream.stream_id)
}

fn accepted_entries(conn: &Connection, stream_id: [u8; 32]) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut stmt = conn.prepare(
        "SELECT signed_bytes FROM table_sync_entries
          WHERE stream_id = ?1
          ORDER BY lamport, device_fingerprint, entry_hash",
    )?;
    Ok(stmt
        .query_map([stream_id.as_slice()], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn ingest_against(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    signed_bytes: &[u8],
    now_ms: i64,
    registry: &[TableSpec],
) -> anyhow::Result<TableSyncIngestOutcome> {
    if !validate_stream_against(conn, account_id, stream, registry)? {
        return Ok(TableSyncIngestOutcome::NoChange);
    }
    let Ok(signed) = crate::entry::decode_signed(signed_bytes) else {
        return Ok(TableSyncIngestOutcome::NoChange);
    };
    if signed.entry.stream_id.to_bytes() != stream.stream_id {
        return Ok(TableSyncIngestOutcome::NoChange);
    }
    let signer = signed.entry.device_fingerprint;
    let Some(pubkey_bytes) =
        account::stored_device_pubkeys(conn, account_id)?.get(&signer).copied()
    else {
        return Ok(TableSyncIngestOutcome::NoChange);
    };
    let Ok(pubkey) = DevicePublic::from_bytes(&pubkey_bytes) else {
        return Ok(TableSyncIngestOutcome::NoChange);
    };
    if crate::entry::verify_signed(signed_bytes, &pubkey).is_err() {
        return Ok(TableSyncIngestOutcome::NoChange);
    }
    let local = crate::load_local_device(conn)?
        .context("table sync requires an existing local device identity")?;
    let registry = repo_registry(registry);
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let ctx = SyncCtx {
        repo_id: &stream.repo_id,
        account_id,
        incarnation_ref: stream.incarnation_ref,
        device: &local,
        registry: &registry,
        now_ms,
    };
    let report = engine::ingest(&tx, &ctx, &stream.scope_id, signed_bytes, &pubkey)?;
    tx.commit()?;
    Ok(match report.outcome {
        IngestOutcome::Applied
        | IngestOutcome::Retained(_)
        | IngestOutcome::AwaitingPredecessor
        | IngestOutcome::Quarantined(_) => TableSyncIngestOutcome::Stored,
        IngestOutcome::AlreadyPresent
        | IngestOutcome::AlreadyAwaiting
        | IngestOutcome::HeldChainFull
        | IngestOutcome::Forked
        | IngestOutcome::AbandonedBehindFork
        | IngestOutcome::Unauthorized => TableSyncIngestOutcome::NoChange,
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::table_sync::registry::{ColumnSpec, ValueType};

    const INCARNATION: [u8; 32] = [0x24; 32];

    fn account() -> AccountId {
        AccountId::from_bytes([0x42; 32])
    }

    const REPO_SPEC: TableSpec = TableSpec {
        name: "t_transport",
        scope_id: "anchors/1",
        spec_version: 1,
        pk: &[
            ColumnSpec::required("repo_id", ValueType::Text),
            ColumnSpec::required("id", ValueType::Text),
        ],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &[],
        repo_column: Some("repo_id"),
    };
    const GLOBAL_SPEC: TableSpec = TableSpec {
        name: "t_global",
        scope_id: "global/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };

    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        crate::local_device(&conn, 0).unwrap();
        conn.execute(
            "INSERT INTO account_repo_incarnation_current(account_id, repository_id, \
             incarnation_ref)
             VALUES (?1, 'repo-a', ?2), (?1, 'repo-b', ?2)",
            params![account().to_bytes().as_slice(), INCARNATION.as_slice()],
        )
        .unwrap();
        conn
    }

    #[test]
    fn supported_streams_are_current_repo_scoped_and_account_global_specs_are_ignored() {
        let conn = database();
        let streams =
            supported_streams_against(&conn, account(), &[REPO_SPEC, GLOBAL_SPEC]).unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].repo_id, "repo-a");
        assert_eq!(streams[1].repo_id, "repo-b");
        assert!(streams.iter().all(|stream| stream.scope_id == "anchors/1"));
        assert!(streams.iter().all(|stream| {
            validate_stream_against(&conn, account(), stream, &[REPO_SPEC, GLOBAL_SPEC]).unwrap()
        }));
        assert!(supported_streams_against(&conn, account(), &[GLOBAL_SPEC]).unwrap().is_empty());
    }

    #[test]
    fn local_authority_rejects_stale_unknown_and_forged_routes_without_writes() {
        let conn = database();
        let current = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
        let mut stale = current.clone();
        stale.incarnation_ref = [9; 32];
        let mut unknown = current.clone();
        unknown.scope_id = "unknown/1".into();
        let mut forged = current.clone();
        forged.stream_id = [8; 32];
        for route in [&stale, &unknown, &forged] {
            assert!(!validate_stream_against(&conn, account(), route, &[REPO_SPEC]).unwrap());
            assert_eq!(
                ingest_against(&conn, account(), route, &[0], 0, &[REPO_SPEC]).unwrap(),
                TableSyncIngestOutcome::NoChange,
            );
        }
        assert!(
            !validate_stream_against(&conn, AccountId::from_bytes([0x43; 32]), &current, &[
                REPO_SPEC
            ],)
            .unwrap(),
            "a sibling account cannot adopt this account's repo stream",
        );
        for table in [
            "table_sync_entries",
            "table_sync_gapped_entries",
            "sync_row_clocks",
            "table_sync_streams",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0, "invalid routes write no rows to {table}");
        }
    }

    #[test]
    fn accepted_snapshot_excludes_gapped_rows() {
        let conn = database();
        let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
        conn.execute(
            "INSERT INTO table_sync_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, signed_bytes, received_at_ms)
             VALUES (?1, ?2, ?3, 0, x'01', 0)",
            params![[1u8; 32].as_slice(), stream.stream_id.as_slice(), [2u8; 32].as_slice()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO table_sync_gapped_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                 gapped_at_ms)
             VALUES (?1, ?2, ?3, 1, ?4, x'02', 0)",
            params![
                [3u8; 32].as_slice(),
                stream.stream_id.as_slice(),
                [2u8; 32].as_slice(),
                [1u8; 32].as_slice(),
            ],
        )
        .unwrap();
        assert_eq!(accepted_entries(&conn, stream.stream_id).unwrap(), vec![vec![1]]);
    }

    #[test]
    fn empty_production_registry_has_no_streams() {
        let conn = database();
        assert!(table_sync_supported_streams(&conn, account()).unwrap().is_empty());
    }

    #[test]
    fn accepted_transfer_is_idempotent_and_obeys_the_current_roster_write_gate() {
        fn add_repo_state(conn: &Connection, account: AccountId) {
            conn.execute_batch(
                "CREATE TABLE t_transport(
                     repo_id TEXT NOT NULL,
                     id TEXT NOT NULL,
                     title TEXT NOT NULL,
                     PRIMARY KEY(repo_id, id)
                 ) STRICT;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO account_repo_incarnation_current(
                     account_id, repository_id, incarnation_ref
                 ) VALUES (?1, 'repo-a', ?2)",
                params![account.to_bytes().as_slice(), INCARNATION.as_slice()],
            )
            .unwrap();
        }

        let mut source = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&source, &crate::test_hooks()).unwrap();
        let account = crate::local_account(&source, 0).unwrap();
        add_repo_state(&source, account);
        source
            .execute(
                "INSERT INTO t_transport(repo_id, id, title) VALUES ('repo-a', 'r1', 'one')",
                [],
            )
            .unwrap();
        let local = crate::load_local_device(&source).unwrap().unwrap();
        let tx = source.transaction().unwrap();
        let ctx = SyncCtx {
            repo_id: "repo-a",
            account_id: account,
            incarnation_ref: INCARNATION,
            device: &local,
            registry: &[REPO_SPEC],
            now_ms: 0,
        };
        let authored = engine::produce_and_author(&tx, &ctx).unwrap();
        tx.commit().unwrap();
        assert_eq!(authored.len(), 1);
        let route = supported_streams_against(&source, account, &[REPO_SPEC]).unwrap().remove(0);

        let restore = |remove_writer: bool| {
            let dest = Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&dest, &crate::test_hooks()).unwrap();
            crate::local_device(&dest, 0).unwrap();
            for entry in crate::account_entries_for_sync(&source, account).unwrap() {
                crate::account_ingest(&dest, &entry.signed_bytes, 0).unwrap();
            }
            add_repo_state(&dest, account);
            if remove_writer {
                dest.execute(
                    "UPDATE account_roster_history SET closed_at = 1 WHERE account_id = ?1",
                    [account.to_bytes().as_slice()],
                )
                .unwrap();
            }
            dest
        };

        let destination = restore(false);
        assert_eq!(
            ingest_against(&destination, account, &route, &authored[0], 1, &[REPO_SPEC]).unwrap(),
            TableSyncIngestOutcome::Stored,
        );
        assert_eq!(
            ingest_against(&destination, account, &route, &authored[0], 2, &[REPO_SPEC]).unwrap(),
            TableSyncIngestOutcome::NoChange,
        );
        let title: String = destination
            .query_row(
                "SELECT title FROM t_transport WHERE repo_id = 'repo-a' AND id = 'r1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "one");

        let removed = restore(true);
        assert_eq!(
            ingest_against(&removed, account, &route, &authored[0], 1, &[REPO_SPEC]).unwrap(),
            TableSyncIngestOutcome::NoChange,
        );
        let accepted: i64 = removed
            .query_row("SELECT COUNT(*) FROM table_sync_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(accepted, 0, "a removed writer reaches no accepted table history");
    }
}
