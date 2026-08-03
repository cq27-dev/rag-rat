//! Production transport seams for the `/5` table-sync engine.
//!
//! The transport sees only current repo-scoped streams and accepted history. Gapped rows remain a
//! local chain-repair detail and are never advertised or transferred.

use std::collections::BTreeSet;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::engine::{self, IngestOutcome, SyncCtx};
use super::registry::{SYNCABLE_TABLES, TableSpec};
use super::scope_stream::scope_stream_id;
use super::{retention, store};
use crate::AccountId;
use crate::account::{self, RepoIncarnationState};
use crate::device::DevicePublic;
use crate::stream::StreamId;

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

/// The accepted tip of one device chain in a table stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSyncChainHead {
    pub device_fingerprint: [u8; 32],
    pub lamport: u64,
    pub entry_hash: [u8; 32],
    /// The retained floor this chain was compacted below, if any (#1127). Routing advice only:
    /// a receiver with no local chain may accept the floor entry as its local root; a receiver
    /// with chain state ignores it entirely.
    pub floor: Option<(u64, [u8; 32])>,
}

/// Durable receiver progress for one offered device chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSyncFrontier {
    Empty,
    /// Entries strictly after this accepted tail are missing.
    Accepted {
        lamport: u64,
        entry_hash: [u8; 32],
    },
    /// Repository purge retained this witness but removed the accepted tip itself. The witnessed
    /// entry must be offered inclusively to restore local authoring continuity.
    Restore {
        lamport: u64,
        entry_hash: [u8; 32],
    },
}

/// Where a causal page of one device chain starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSyncEntryStart {
    Beginning,
    After { lamport: u64, entry_hash: [u8; 32] },
    At { lamport: u64, entry_hash: [u8; 32] },
}

/// One accepted entry plus the cursor needed to request the next page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSyncChainEntry {
    pub lamport: u64,
    pub entry_hash: [u8; 32],
    pub signed_bytes: Vec<u8>,
}

/// Current repo-scoped streams supported by the production registry.
pub fn table_sync_supported_streams(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<TableSyncStream>> {
    supported_streams_against(conn, account_id, SYNCABLE_TABLES)
}

/// Author every unpublished local row in the production registry before a table manifest is built.
///
/// The count includes entries authored by re-adoption (#997): a removal drain can be the only
/// thing this pass authors, and a caller using the return as "did anything change" must see it.
pub fn table_sync_author_pending(
    conn: &Connection,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<usize> {
    let streams = supported_streams_against(conn, account_id, SYNCABLE_TABLES)?;
    let repos: BTreeSet<(String, [u8; 32])> =
        streams.into_iter().map(|stream| (stream.repo_id, stream.incarnation_ref)).collect();
    if repos.is_empty() {
        return Ok(0);
    }

    let device = crate::local_device(conn, now_ms)?;
    if !account::device_is_effective_writer(conn, account_id, device.fingerprint())? {
        return Ok(0);
    }
    let _durability = crate::AuthoredDurability::begin(conn)?;
    let mut authored = 0;
    for (repo_id, incarnation_ref) in repos {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let ctx = SyncCtx {
            repo_id: &repo_id,
            account_id,
            incarnation_ref,
            device: &device,
            registry: SYNCABLE_TABLES,
            now_ms,
        };
        // INVARIANT: the account fold enqueues re-adoption work for EVERY stream in
        // table_sync_streams, while this drain only walks repo-scoped specs' streams. The two
        // sets coincide as long as every registered production table is repo-scoped — registering
        // an account-scoped table must derive the drain set from the work table instead.
        let mut streams: Vec<crate::stream::StreamId> = SYNCABLE_TABLES
            .iter()
            .filter(|spec| spec.repo_column.is_some())
            .map(|spec| scope_stream_id(&repo_id, account_id, incarnation_ref, spec.scope_id))
            .collect();
        authored += engine::produce_and_author(&tx, &ctx)?.len();
        streams.sort_unstable();
        streams.dedup();
        for stream in streams {
            // Drain EVERY pending removal, not one: two devices removed on one stream must not
            // wait a whole sync session for the second repair. Each call completes one removal, so
            // `has_pending` makes progress — UNLESS the pass cannot drain: a row whose synced
            // column is unreadable today (retried once the cell is repaired), or a stream with no
            // recorded apply context. `None` is that case: stop rather than spin on a work item
            // this pass cannot finish.
            while store::has_pending_readoption_work(&tx, account_id, stream)? {
                let Some(reauthored) =
                    engine::process_readoption_work_for_stream(&tx, &ctx, stream)?
                else {
                    break;
                };
                authored += reauthored;
            }
        }
        tx.commit()?;
    }
    Ok(authored)
}

/// Compact accepted chain prefixes for scopes whose retention policy bounds them (#1127).
///
/// `retain` answers the per-chain accepted-entry budget for a scope, `None` for full retention
/// (anchors/1 stays unbounded). A chain with more RECLAIMABLE entries than its budget compacts to
/// keep its newest; chains within budget are untouched. Entries retained for forward-compat
/// replay (`pending_reason IS NOT NULL`) are neither counted nor dropped — a chain pinned above
/// budget by pending entries makes the computed floor not advance, which is a steady-state no-op,
/// never an error, so the driver is safely re-runnable on any cadence. Each stream compacts in
/// its own IMMEDIATE transaction, and the caller's authoring pass already ran, so a winner
/// restamp never disowns an unsent local edit (see the retention module docs).
///
/// Returns the number of accepted entries reclaimed.
pub fn table_sync_compact_overdue(
    conn: &Connection,
    account_id: AccountId,
    now_ms: i64,
    retain: &dyn Fn(&str) -> Option<u64>,
) -> anyhow::Result<usize> {
    let streams = supported_streams_against(conn, account_id, SYNCABLE_TABLES)?;
    let registry = repo_registry(SYNCABLE_TABLES);
    let mut compacted = 0;
    for stream in streams {
        let Some(keep) = retain(&stream.scope_id) else {
            continue;
        };
        anyhow::ensure!(keep >= 1, "a retention budget of zero keeps no chain tail to build on");
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let chains = {
            let mut stmt = tx.prepare(
                "SELECT device_fingerprint, COUNT(*) FROM table_sync_entries
                 WHERE stream_id = ?1 AND pending_reason IS NULL
                 GROUP BY device_fingerprint",
            )?;
            stmt.query_map([stream.stream_id.as_slice()], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (device_bytes, count) in chains {
            let count = u64::try_from(count)?;
            if count <= keep {
                continue;
            }
            // The floor is the oldest RECLAIMABLE entry to retain: the one at offset
            // (count - keep) in lamport order. compact_chain_prefix drops strictly below it.
            let floor_lamport: i64 = tx.query_row(
                "SELECT lamport FROM table_sync_entries
                 WHERE stream_id = ?1 AND device_fingerprint = ?2 AND pending_reason IS NULL
                 ORDER BY lamport LIMIT 1 OFFSET ?3",
                params![
                    stream.stream_id.as_slice(),
                    device_bytes.as_slice(),
                    i64::try_from(count - keep)?
                ],
                |row| row.get(0),
            )?;
            let floor_lamport = u64::try_from(floor_lamport)?;
            let device = crate::op::DeviceFingerprint::from_bytes(fixed32(device_bytes)?);
            // A pending entry below the computed floor would survive locally yet become
            // unreachable to floor-adopting peers (served early it parks and is swept; served
            // late it reads as already-present) — silent loss of the forward-compat payload it
            // exists to retain. Clamp the floor at the lowest pending lamport, so every pending
            // entry sits AT or above the floor and stays offerable.
            let lowest_pending: Option<i64> = tx
                .query_row(
                    "SELECT MIN(lamport) FROM table_sync_entries
                     WHERE stream_id = ?1 AND device_fingerprint = ?2
                       AND pending_reason IS NOT NULL",
                    params![stream.stream_id.as_slice(), device.to_bytes().as_slice()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let floor_lamport = match lowest_pending {
                Some(pending) if (pending as u64) < floor_lamport => pending as u64,
                _ => floor_lamport,
            };
            // A chain pinned by pending entries (or simply already compacted past this point)
            // computes a floor that does not advance: steady state, not the caller bug
            // compact_chain_prefix's refusal exists to catch.
            if retention::retained_floor(&tx, StreamId::from_bytes(stream.stream_id), device)?
                .is_some_and(|current| floor_lamport <= current)
            {
                continue;
            }
            let report = retention::compact_chain_prefix(
                &tx,
                StreamId::from_bytes(stream.stream_id),
                device,
                floor_lamport,
                &registry,
                now_ms,
            )?;
            compacted += report.dropped_entries;
        }
        tx.commit()?;
    }
    Ok(compacted)
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

/// A bounded canonical page of device chains after `after_device`.
pub fn table_sync_chain_page_after(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    after_device: Option<[u8; 32]>,
    limit: usize,
) -> anyhow::Result<Vec<TableSyncChainHead>> {
    if limit == 0 || !table_sync_validate_stream(conn, account_id, stream)? {
        return Ok(Vec::new());
    }
    accepted_chain_page(conn, stream.stream_id, after_device, limit)
}

/// Durable progress for one device chain in a validated current stream.
pub fn table_sync_chain_frontier(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    device_fingerprint: [u8; 32],
) -> anyhow::Result<TableSyncFrontier> {
    if !table_sync_validate_stream(conn, account_id, stream)? {
        return Ok(TableSyncFrontier::Empty);
    }
    chain_frontier(conn, stream.stream_id, device_fingerprint)
}

/// A bounded causal page from one accepted device chain. Gapped rows are never transferred.
pub fn table_sync_chain_entries(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    device_fingerprint: [u8; 32],
    start: TableSyncEntryStart,
    limit: usize,
) -> anyhow::Result<Vec<TableSyncChainEntry>> {
    if limit == 0 || !table_sync_validate_stream(conn, account_id, stream)? {
        return Ok(Vec::new());
    }
    accepted_chain_entries(conn, stream.stream_id, device_fingerprint, start, limit)
}

/// Feed one untrusted signed envelope through the existing table-sync authority, chain and payload
/// gates. Invalid/stale routes are skipped before a transaction can write any table-sync state.
pub fn table_sync_ingest(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    expected_device: [u8; 32],
    signed_bytes: &[u8],
    now_ms: i64,
    advertised_floor: Option<(u64, [u8; 32])>,
) -> anyhow::Result<TableSyncIngestOutcome> {
    ingest_against(
        conn,
        account_id,
        stream,
        expected_device,
        signed_bytes,
        now_ms,
        advertised_floor,
        SYNCABLE_TABLES,
    )
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

fn accepted_chain_page(
    conn: &Connection,
    stream_id: [u8; 32],
    after_device: Option<[u8; 32]>,
    limit: usize,
) -> anyhow::Result<Vec<TableSyncChainHead>> {
    let mut stmt = conn.prepare(
        "SELECT e.device_fingerprint, e.lamport, e.entry_hash, f.lamport, f.entry_hash
           FROM table_sync_entries e
           LEFT JOIN table_sync_retained_floors f
             ON f.stream_id = e.stream_id
            AND f.device_fingerprint = e.device_fingerprint
          WHERE e.stream_id = ?1
            AND (?2 IS NULL OR e.device_fingerprint > ?2)
            AND NOT EXISTS (
                SELECT 1 FROM table_sync_entries newer
                 WHERE newer.stream_id = e.stream_id
                   AND newer.device_fingerprint = e.device_fingerprint
                   AND newer.lamport > e.lamport
            )
          ORDER BY e.device_fingerprint
          LIMIT ?3",
    )?;
    stmt.query_map(
        params![
            stream_id.as_slice(),
            after_device.map(|device| device.to_vec()),
            i64::try_from(limit)?,
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
            ))
        },
    )?
    .map(|row| {
        let (device, lamport, hash, floor_lamport, floor_hash) = row?;
        let floor = floor_lamport
            .map(|l| -> anyhow::Result<_> {
                Ok((
                    u64::try_from(l)?,
                    fixed32(floor_hash.expect("floor row carries its entry hash"))?,
                ))
            })
            .transpose()?;
        Ok(TableSyncChainHead {
            device_fingerprint: fixed32(device)?,
            lamport: u64::try_from(lamport)?,
            entry_hash: fixed32(hash)?,
            floor,
        })
    })
    .collect::<anyhow::Result<_>>()
}

fn chain_frontier(
    conn: &Connection,
    stream_id: [u8; 32],
    device_fingerprint: [u8; 32],
) -> anyhow::Result<TableSyncFrontier> {
    let accepted = accepted_chain_tail(conn, stream_id, device_fingerprint)?;
    let witness = conn
        .query_row(
            "SELECT lamport, entry_hash FROM table_sync_chain_tips
              WHERE stream_id = ?1 AND device_fingerprint = ?2",
            params![stream_id.as_slice(), device_fingerprint.as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
        .map(|(lamport, hash)| -> anyhow::Result<_> {
            Ok((u64::try_from(lamport)?, fixed32(hash)?))
        })
        .transpose()?;
    match (accepted, witness) {
        (None, None) => Ok(TableSyncFrontier::Empty),
        (Some((lamport, entry_hash)), None) =>
            Ok(TableSyncFrontier::Accepted { lamport, entry_hash }),
        (None, Some((lamport, entry_hash))) =>
            Ok(TableSyncFrontier::Restore { lamport, entry_hash }),
        (Some(accepted), Some(witness)) if accepted == witness =>
            Ok(TableSyncFrontier::Accepted { lamport: accepted.0, entry_hash: accepted.1 }),
        (Some(accepted), Some(witness)) if witness.0 > accepted.0 =>
            Ok(TableSyncFrontier::Restore { lamport: witness.0, entry_hash: witness.1 }),
        (Some(accepted), Some(witness)) => anyhow::bail!(
            "table-sync chain tip witness {witness:?} conflicts with accepted tail {accepted:?}"
        ),
    }
}

fn accepted_chain_tail(
    conn: &Connection,
    stream_id: [u8; 32],
    device_fingerprint: [u8; 32],
) -> anyhow::Result<Option<(u64, [u8; 32])>> {
    conn.query_row(
        "SELECT lamport, entry_hash FROM table_sync_entries
          WHERE stream_id = ?1 AND device_fingerprint = ?2
          ORDER BY lamport DESC LIMIT 1",
        params![stream_id.as_slice(), device_fingerprint.as_slice()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )
    .optional()?
    .map(|(lamport, hash)| -> anyhow::Result<_> { Ok((u64::try_from(lamport)?, fixed32(hash)?)) })
    .transpose()
}

fn accepted_chain_entries(
    conn: &Connection,
    stream_id: [u8; 32],
    device_fingerprint: [u8; 32],
    start: TableSyncEntryStart,
    limit: usize,
) -> anyhow::Result<Vec<TableSyncChainEntry>> {
    let (minimum_lamport, inclusive) = match start {
        TableSyncEntryStart::Beginning => (None, false),
        TableSyncEntryStart::After { lamport, entry_hash } => {
            anyhow::ensure!(
                cursor_matches(conn, stream_id, device_fingerprint, lamport, entry_hash)?,
                "table-sync accepted chain cursor is not present locally"
            );
            (Some(lamport), false)
        },
        TableSyncEntryStart::At { lamport, entry_hash } => {
            if cursor_matches(conn, stream_id, device_fingerprint, lamport, entry_hash)? {
                (Some(lamport), true)
            } else {
                let successor = direct_successor_lamport(
                    conn,
                    stream_id,
                    device_fingerprint,
                    lamport,
                    entry_hash,
                )?;
                let Some(successor) = successor else {
                    anyhow::bail!(
                        "table-sync restore cursor has neither its tip nor a direct successor"
                    )
                };
                (Some(successor), true)
            }
        },
    };
    let comparison = if inclusive { ">=" } else { ">" };
    let sql = format!(
        "SELECT lamport, entry_hash, signed_bytes FROM table_sync_entries
          WHERE stream_id = ?1 AND device_fingerprint = ?2
            AND (?3 IS NULL OR lamport {comparison} ?3)
          ORDER BY lamport LIMIT ?4"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(
        params![
            stream_id.as_slice(),
            device_fingerprint.as_slice(),
            minimum_lamport.map(i64::try_from).transpose()?,
            i64::try_from(limit)?,
        ],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?)),
    )?
    .map(|row| {
        let (lamport, hash, signed_bytes) = row?;
        Ok(TableSyncChainEntry {
            lamport: u64::try_from(lamport)?,
            entry_hash: fixed32(hash)?,
            signed_bytes,
        })
    })
    .collect::<anyhow::Result<_>>()
}

fn direct_successor_lamport(
    conn: &Connection,
    stream_id: [u8; 32],
    device_fingerprint: [u8; 32],
    witness_lamport: u64,
    entry_hash: [u8; 32],
) -> anyhow::Result<Option<u64>> {
    conn.query_row(
        "SELECT lamport FROM table_sync_entries
          WHERE stream_id = ?1 AND device_fingerprint = ?2 AND prev_hash = ?3 AND lamport > ?4
          ORDER BY lamport LIMIT 1",
        params![
            stream_id.as_slice(),
            device_fingerprint.as_slice(),
            entry_hash.as_slice(),
            i64::try_from(witness_lamport)?,
        ],
        |row| row.get::<_, i64>(0),
    )
    .optional()?
    .map(u64::try_from)
    .transpose()
    .map_err(Into::into)
}

fn cursor_matches(
    conn: &Connection,
    stream_id: [u8; 32],
    device_fingerprint: [u8; 32],
    lamport: u64,
    entry_hash: [u8; 32],
) -> anyhow::Result<bool> {
    let stored = conn
        .query_row(
            "SELECT entry_hash FROM table_sync_entries
              WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport = ?3",
            params![stream_id.as_slice(), device_fingerprint.as_slice(), i64::try_from(lamport)?,],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    let Some(stored) = stored else { return Ok(false) };
    anyhow::ensure!(
        fixed32(stored)? == entry_hash,
        "table-sync chain cursor hash conflicts at lamport {lamport}"
    );
    Ok(true)
}

fn fixed32(bytes: Vec<u8>) -> anyhow::Result<[u8; 32]> {
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        anyhow::anyhow!("stored table-sync hash must be 32 bytes, got {}", bytes.len())
    })
}

#[allow(clippy::too_many_arguments)]
fn ingest_against(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    expected_device: [u8; 32],
    signed_bytes: &[u8],
    now_ms: i64,
    advertised_floor: Option<(u64, [u8; 32])>,
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
    if signer.to_bytes() != expected_device {
        return Ok(TableSyncIngestOutcome::NoChange);
    }
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
    let report = engine::ingest(
        &tx,
        &ctx,
        &stream.scope_id,
        signed_bytes,
        &pubkey,
        advertised_floor
            .map(|(lamport, entry_hash)| store::AdvertisedFloor { lamport, entry_hash }),
    )?;
    if registry.iter().any(|spec| spec.name == "repo_memory_bindings")
        && std::iter::once(&report.outcome)
            .chain(report.promoted.iter())
            .any(|outcome| matches!(outcome, IngestOutcome::Applied))
        && rag_rat_db::schema::repo_id_is_registered(&tx, &stream.repo_id)?
    {
        rag_rat_db::meta::bump_lens_revisions(&tx, &stream.repo_id, &[
            rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
            rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
        ])?;
    }
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
                ingest_against(&conn, account(), route, [0; 32], &[0], 0, None, &[REPO_SPEC])
                    .unwrap(),
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
    fn the_compaction_driver_is_idempotent_past_pending_entries() {
        let conn = database();
        let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
        conn.execute(
            "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, incarnation_ref, \
             scope_id) VALUES (?1, 'repo-a', ?2, ?3, 'anchors/1')",
            params![
                stream.stream_id.as_slice(),
                account().to_bytes().as_slice(),
                INCARNATION.as_slice()
            ],
        )
        .unwrap();
        for lamport in 0..5i64 {
            conn.execute(
                "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, x'00', 0)",
                params![
                    [(lamport + 1) as u8; 32].as_slice(),
                    stream.stream_id.as_slice(),
                    [2u8; 32].as_slice(),
                    lamport
                ],
            )
            .unwrap();
        }
        // The tip carries a pending mark: reclaimable entries pin below it.
        conn.execute(
            "UPDATE table_sync_entries SET pending_reason = 'unknown_column' WHERE lamport = 4",
            [],
        )
        .unwrap();

        let keep_one = &|_scope: &str| Some(1);
        let first = table_sync_compact_overdue(&conn, account(), 1, keep_one).unwrap();
        assert_eq!(first, 3, "reclaimable entries 0..3 drop; the pending tip survives");
        let floor: Option<i64> = conn
            .query_row("SELECT lamport FROM table_sync_retained_floors", [], |row| row.get(0))
            .ok();
        assert_eq!(floor, Some(3));

        let second = table_sync_compact_overdue(&conn, account(), 2, keep_one).unwrap();
        assert_eq!(second, 0, "a chain pinned by its pending tip computes a non-advancing floor");
        let remaining: Vec<i64> = conn
            .prepare("SELECT lamport FROM table_sync_entries ORDER BY lamport")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(remaining, vec![3, 4], "nothing moves on the second pass");

        let zero = &|_scope: &str| Some(0);
        assert!(
            table_sync_compact_overdue(&conn, account(), 3, zero).is_err(),
            "a zero budget would drop the chain tail itself — refused"
        );
    }

    /// A pending entry below the computed floor would survive locally yet be unreachable to
    /// floor-adopting peers; the driver clamps the floor at the lowest pending lamport.
    #[test]
    fn the_compaction_driver_clamps_the_floor_above_pending_entries() {
        let conn = database();
        let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
        conn.execute(
            "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, incarnation_ref, \
             scope_id) VALUES (?1, 'repo-a', ?2, ?3, 'anchors/1')",
            params![
                stream.stream_id.as_slice(),
                account().to_bytes().as_slice(),
                INCARNATION.as_slice()
            ],
        )
        .unwrap();
        for lamport in 0..5i64 {
            conn.execute(
                "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, x'00', 0)",
                params![
                    [(lamport + 1) as u8; 32].as_slice(),
                    stream.stream_id.as_slice(),
                    [2u8; 32].as_slice(),
                    lamport
                ],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE table_sync_entries SET pending_reason = 'unknown_column' WHERE lamport = 1",
            [],
        )
        .unwrap();

        let keep_two = &|_scope: &str| Some(2);
        let compacted = table_sync_compact_overdue(&conn, account(), 1, keep_two).unwrap();
        assert_eq!(compacted, 1, "only the genesis drops: the floor clamps at the pending entry");
        let floor: Option<i64> = conn
            .query_row("SELECT lamport FROM table_sync_retained_floors", [], |row| row.get(0))
            .ok();
        assert_eq!(floor, Some(1), "the pending entry sits AT the floor and stays offerable");
        let remaining: Vec<i64> = conn
            .prepare("SELECT lamport FROM table_sync_entries ORDER BY lamport")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(remaining, vec![1, 2, 3, 4]);
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
        let accepted = accepted_chain_entries(
            &conn,
            stream.stream_id,
            [2; 32],
            TableSyncEntryStart::Beginning,
            10,
        )
        .unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].signed_bytes, vec![1]);
    }

    #[test]
    fn accepted_device_chains_page_canonically_and_resume_from_exact_frontiers() {
        let conn = database();
        let stream = supported_streams_against(&conn, account(), &[REPO_SPEC]).unwrap().remove(0);
        for (device, lamport, hash) in [(2u8, 1i64, 11u8), (2, 3, 13), (4, 2, 22)] {
            conn.execute(
                "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                params![
                    [hash; 32].as_slice(),
                    stream.stream_id.as_slice(),
                    [device; 32].as_slice(),
                    lamport,
                    vec![hash],
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO table_sync_chain_tips(
                     stream_id, device_fingerprint, lamport, entry_hash)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(stream_id, device_fingerprint) DO UPDATE SET
                     lamport = excluded.lamport, entry_hash = excluded.entry_hash
                 WHERE excluded.lamport > table_sync_chain_tips.lamport",
                params![
                    stream.stream_id.as_slice(),
                    [device; 32].as_slice(),
                    lamport,
                    [hash; 32].as_slice(),
                ],
            )
            .unwrap();
        }

        let first = accepted_chain_page(&conn, stream.stream_id, None, 1).unwrap();
        assert_eq!(first, vec![TableSyncChainHead {
            device_fingerprint: [2; 32],
            lamport: 3,
            entry_hash: [13; 32],
            floor: None,
        }]);
        let second = accepted_chain_page(&conn, stream.stream_id, Some([2; 32]), 1).unwrap();
        assert_eq!(second[0].device_fingerprint, [4; 32]);
        assert_eq!(
            chain_frontier(&conn, stream.stream_id, [2; 32]).unwrap(),
            TableSyncFrontier::Accepted { lamport: 3, entry_hash: [13; 32] }
        );

        let page = accepted_chain_entries(
            &conn,
            stream.stream_id,
            [2; 32],
            TableSyncEntryStart::Beginning,
            1,
        )
        .unwrap();
        assert_eq!(page[0].signed_bytes, vec![11]);
        let suffix = accepted_chain_entries(
            &conn,
            stream.stream_id,
            [2; 32],
            TableSyncEntryStart::After { lamport: 1, entry_hash: [11; 32] },
            10,
        )
        .unwrap();
        assert_eq!(suffix.iter().map(|entry| entry.signed_bytes[0]).collect::<Vec<_>>(), [13]);
        assert!(
            accepted_chain_entries(
                &conn,
                stream.stream_id,
                [2; 32],
                TableSyncEntryStart::After { lamport: 1, entry_hash: [99; 32] },
                10,
            )
            .unwrap_err()
            .to_string()
            .contains("cursor hash conflicts")
        );
    }

    #[test]
    fn a_witness_without_an_accepted_tail_requests_the_tip_inclusively() {
        let source = database();
        let destination = database();
        let source_stream =
            supported_streams_against(&source, account(), &[REPO_SPEC]).unwrap().remove(0);
        let destination_stream =
            supported_streams_against(&destination, account(), &[REPO_SPEC]).unwrap().remove(0);
        let device = [2; 32];
        let tip = [9; 32];
        source
            .execute(
                "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, signed_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, 7, x'09', 0)",
                params![tip.as_slice(), source_stream.stream_id.as_slice(), device.as_slice()],
            )
            .unwrap();
        for (conn, stream) in [(&source, &source_stream), (&destination, &destination_stream)] {
            conn.execute(
                "INSERT INTO table_sync_chain_tips(
                     stream_id, device_fingerprint, lamport, entry_hash)
                 VALUES (?1, ?2, 7, ?3)",
                params![stream.stream_id.as_slice(), device.as_slice(), tip.as_slice()],
            )
            .unwrap();
        }

        let frontier = chain_frontier(&destination, destination_stream.stream_id, device).unwrap();
        assert_eq!(frontier, TableSyncFrontier::Restore { lamport: 7, entry_hash: tip });
        let TableSyncFrontier::Restore { lamport, entry_hash } = frontier else { unreachable!() };
        let restored = accepted_chain_entries(
            &source,
            source_stream.stream_id,
            device,
            TableSyncEntryStart::At { lamport, entry_hash },
            1,
        )
        .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].entry_hash, tip);

        let successor_source = database();
        let successor_stream =
            supported_streams_against(&successor_source, account(), &[REPO_SPEC])
                .unwrap()
                .remove(0);
        successor_source
            .execute(
                "INSERT INTO table_sync_entries(
                     entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                     received_at_ms)
                 VALUES (?1, ?2, ?3, 8, ?4, x'0a', 0)",
                params![
                    [10u8; 32].as_slice(),
                    successor_stream.stream_id.as_slice(),
                    device.as_slice(),
                    tip.as_slice(),
                ],
            )
            .unwrap();
        let successor = accepted_chain_entries(
            &successor_source,
            successor_stream.stream_id,
            device,
            TableSyncEntryStart::At { lamport, entry_hash },
            1,
        )
        .unwrap();
        assert_eq!(successor.len(), 1);
        assert_eq!(successor[0].entry_hash, [10; 32]);
    }

    #[test]
    fn production_registry_advertises_one_anchors_stream_per_current_repo() {
        let conn = database();
        let streams = table_sync_supported_streams(&conn, account()).unwrap();
        assert_eq!(streams.len(), 2);
        assert!(streams.iter().all(|stream| stream.scope_id == "anchors/1"));
        assert_eq!(streams.iter().map(|stream| stream.repo_id.as_str()).collect::<Vec<_>>(), [
            "repo-a", "repo-b"
        ]);
    }

    #[test]
    fn production_anchors_create_rebind_and_delete_preserve_local_resolution() {
        let source = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&source, &crate::test_hooks()).unwrap();
        source
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms)
                 VALUES ('repo-a', 'repo-a', 0)",
                [],
            )
            .unwrap();
        let account = crate::local_account(&source, 0).unwrap();
        crate::ensure_repo_incarnation(&source, "repo-a", 1).unwrap().unwrap();
        source
            .execute(
                "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                     logical_symbol_id, symbol_id, chunk_id, edge_id, anchor_status, created_at_ms)
                 VALUES ('repo-a', 'memory-a', 'path', 'src/lib.rs', 'src/lib.rs', 4, 5,
                         11, 12, 13, 14, 'current', 2)",
                [],
            )
            .unwrap();
        assert_eq!(table_sync_author_pending(&source, account, 2).unwrap(), 1);
        let route = table_sync_supported_streams(&source, account).unwrap().remove(0);

        let destination = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&destination, &crate::test_hooks()).unwrap();
        crate::local_device(&destination, 0).unwrap();
        for entry in crate::account_entries_for_sync(&source, account).unwrap() {
            crate::account_ingest(&destination, &entry.signed_bytes, 0).unwrap();
        }
        let lens_revisions = |conn: &Connection| {
            (
                rag_rat_db::meta::repo_meta(
                    conn,
                    "repo-a",
                    rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
                )
                .unwrap(),
                rag_rat_db::meta::repo_meta(
                    conn,
                    "repo-a",
                    rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
                )
                .unwrap(),
            )
        };
        let before_create = lens_revisions(&destination);
        let sync_all = |destination: &Connection| {
            let heads = table_sync_chain_page_after(&source, account, &route, None, 10).unwrap();
            assert_eq!(heads.len(), 1);
            for entry in table_sync_chain_entries(
                &source,
                account,
                &route,
                heads[0].device_fingerprint,
                TableSyncEntryStart::Beginning,
                20,
            )
            .unwrap()
            {
                table_sync_ingest(
                    destination,
                    account,
                    &route,
                    heads[0].device_fingerprint,
                    &entry.signed_bytes,
                    3,
                    None,
                )
                .unwrap();
            }
        };
        sync_all(&destination);
        assert_eq!(lens_revisions(&destination), before_create);
        let replicated_before_registration: bool = destination
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM repo_memory_bindings
                     WHERE repo_id = 'repo-a' AND memory_id = 'memory-a')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(replicated_before_registration);
        destination
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms)
                 VALUES ('repo-a', 'repo-a', 0)",
                [],
            )
            .unwrap();
        destination
            .execute(
                "UPDATE repo_memory_bindings
                 SET logical_symbol_id = 71, symbol_id = 72, chunk_id = 73, edge_id = 74,
                     anchor_status = 'relocated'
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
                [],
            )
            .unwrap();
        assert_eq!(
            table_sync_author_pending(&destination, account, 3).unwrap(),
            0,
            "checkout-local resolution does not become a replicated edit",
        );
        source
            .execute(
                "UPDATE repo_memory_bindings SET path = 'src/renamed.rs', start_line = 6
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
                [],
            )
            .unwrap();
        assert_eq!(table_sync_author_pending(&source, account, 3).unwrap(), 1);
        let before_update = lens_revisions(&destination);
        sync_all(&destination);
        let after_update = lens_revisions(&destination);
        assert_ne!(after_update.0, before_update.0);
        assert_ne!(after_update.1, before_update.1);
        let row: (String, i64, i64, i64, i64, String) = destination
            .query_row(
                "SELECT path, logical_symbol_id, symbol_id, chunk_id, edge_id, anchor_status
                 FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row, ("src/renamed.rs".into(), 71, 72, 73, 74, "relocated".into()));

        source
            .execute_batch(
                "DELETE FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a';
                 INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                     anchor_status, created_at_ms)
                 VALUES ('repo-a', 'memory-a', 'path', 'src/moved.rs', 'src/moved.rs', 8, 9,
                         'current', 4)",
            )
            .unwrap();
        assert_eq!(table_sync_author_pending(&source, account, 4).unwrap(), 2);
        let before_rebind = lens_revisions(&destination);
        sync_all(&destination);
        let after_rebind = lens_revisions(&destination);
        assert_ne!(after_rebind.0, before_rebind.0);
        assert_ne!(after_rebind.1, before_rebind.1);
        let rebound: (String, String) = destination
            .query_row(
                "SELECT path, anchor_status FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rebound, ("src/moved.rs".into(), "unverified".into()));

        source
            .execute(
                "DELETE FROM repo_memory_bindings
                 WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
                [],
            )
            .unwrap();
        assert_eq!(table_sync_author_pending(&source, account, 5).unwrap(), 1);
        let before_delete = lens_revisions(&destination);
        sync_all(&destination);
        let after_delete = lens_revisions(&destination);
        assert_ne!(after_delete.0, before_delete.0);
        assert_ne!(after_delete.1, before_delete.1);
        let exists: bool = destination
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM repo_memory_bindings
                     WHERE repo_id = 'repo-a' AND memory_id = 'memory-a')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!exists);
        assert_eq!(table_sync_author_pending(&source, account, 6).unwrap(), 0);
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
        let author = local.public().fingerprint().to_bytes();
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
            ingest_against(&destination, account, &route, [0; 32], &authored[0], 1, None, &[
                REPO_SPEC
            ])
            .unwrap(),
            TableSyncIngestOutcome::NoChange,
        );
        assert_eq!(
            ingest_against(&destination, account, &route, author, &authored[0], 1, None, &[
                REPO_SPEC
            ])
            .unwrap(),
            TableSyncIngestOutcome::Stored,
        );
        assert_eq!(
            ingest_against(&destination, account, &route, author, &authored[0], 2, None, &[
                REPO_SPEC
            ])
            .unwrap(),
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
            ingest_against(&removed, account, &route, author, &authored[0], 1, None, &[REPO_SPEC])
                .unwrap(),
            TableSyncIngestOutcome::NoChange,
        );
        let accepted: i64 = removed
            .query_row("SELECT COUNT(*) FROM table_sync_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(accepted, 0, "a removed writer reaches no accepted table history");
    }
}
