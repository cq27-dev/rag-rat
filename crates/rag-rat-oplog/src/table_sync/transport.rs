//! Production transport seams for the `/5` table-sync engine.
//!
//! The transport sees only current repo-scoped streams and accepted history. Gapped rows remain a
//! local chain-repair detail and are never advertised or transferred.

use std::collections::BTreeSet;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::apply::LocalWriterMemo;
use super::engine::{self, IngestOutcome, SyncCtx};
use super::registry::{SYNCABLE_TABLES, TableSpec, scope_lens_metas};
use super::scope_stream::{ScopeId, scope_stream_id};
use super::{coverage, diagnostics, retention, store};
use crate::account::{self, RepoIncarnationState};
use crate::device::DevicePublic;
use crate::stream::{EntryHash, StreamId};
use crate::{AccountId, cbor};

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

/// One position in a device chain: an entry's lamport and the entry hash at it. Raw bytes, like the
/// rest of this surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSyncChainCursor {
    pub lamport: u64,
    pub entry_hash: [u8; 32],
}

impl TableSyncChainCursor {
    /// A stored `(lamport, entry_hash)` row.
    fn from_row(lamport: i64, entry_hash: Vec<u8>) -> anyhow::Result<Self> {
        Ok(Self {
            lamport: u64::try_from(lamport)?,
            entry_hash: cbor::sql_fixed(entry_hash, "entry_hash")?,
        })
    }

    fn to_store(self) -> store::ChainCursor {
        store::ChainCursor {
            lamport: self.lamport,
            entry_hash: EntryHash::from_bytes(self.entry_hash),
        }
    }
}

/// One device chain’s offered tip: the accepted tail, or an outstanding suffix tip needed
/// to complete a previously adopted floor. Receive frontiers always use accepted state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSyncChainHead {
    pub device_fingerprint: [u8; 32],
    pub lamport: u64,
    pub entry_hash: [u8; 32],
    /// The retained floor this chain was compacted below, if any (#1127). Routing advice only:
    /// a receiver with no local chain may accept the floor entry as its local root; a receiver
    /// with chain state ignores it entirely.
    pub floor: Option<TableSyncChainCursor>,
}

/// Durable receiver progress for one offered device chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSyncFrontier {
    Empty,
    /// Entries strictly after this accepted tail are missing.
    Accepted(TableSyncChainCursor),
    /// Repository purge retained this witness but removed the accepted tip itself. The witnessed
    /// entry must be offered inclusively to restore local authoring continuity.
    Restore(TableSyncChainCursor),
}

/// Where a causal page of one device chain starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSyncEntryStart {
    Beginning,
    After(TableSyncChainCursor),
    At(TableSyncChainCursor),
}

/// One accepted entry plus the cursor needed to request the next page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSyncChainEntry {
    pub cursor: TableSyncChainCursor,
    pub signed_bytes: Vec<u8>,
}

/// Current repo-scoped streams supported by the production registry.
pub fn table_sync_supported_streams(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<TableSyncStream>> {
    let _snapshot = crate::account::control_policy::read_snapshot(conn)?;
    crate::account::require_supported_account_control(conn, account_id)?;
    supported_streams_against(conn, account_id, SYNCABLE_TABLES)
}

/// Sweep gapped entries older than [`store::GAPPED_ENTRY_MAX_AGE_MS`], with everything held
/// behind them (#1127 slice c). A stalled chain otherwise burns its per-chain cap forever: the
/// predecessor never arrives and nothing reclaims. Expiry is recovery-safe — a peer re-offers
/// from the receiver's accepted frontier, so an entry dropped here is re-retained on arrival.
///
/// Runs from [`table_sync_author_pending`], which is itself the session-prepare hook — a cadence
/// that exists only on WRITER sessions (`can_push`). A pull-only (read-only grant) replica
/// ingests and parks entries without ever running this sweep: its gapped table is bounded by the
/// per-chain cap alone. Named rather than fixed — if receive-only replicas become a real shape,
/// the horizon needs a read-path cadence of its own.
///
/// The scan reads the whole gapped table once per prepare; no index, deliberately — the table is
/// bounded at the per-chain cap times bounded chains, and the sweep runs on the session cadence,
/// not per query. Returns the number of entries discarded.
pub(crate) fn table_sync_sweep_expired_gapped(
    conn: &Connection,
    now_ms: i64,
) -> anyhow::Result<usize> {
    let cutoff = now_ms.saturating_sub(store::GAPPED_ENTRY_MAX_AGE_MS);
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let expired: Vec<(Vec<u8>, Vec<u8>)> = {
        let mut stmt = tx.prepare(
            "SELECT stream_id, entry_hash FROM table_sync_gapped_entries
             WHERE gapped_at_ms < ?1",
        )?;
        stmt.query_map([cutoff], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut swept = 0;
    for (stream_id, hash) in expired {
        let hash = EntryHash::try_from_sql(hash)?;
        // The descendant sweep below may have already reclaimed this row behind an earlier root;
        // only a real deletion counts (and only a real root needs its subtree walked).
        if tx.execute("DELETE FROM table_sync_gapped_entries WHERE entry_hash = ?1", params![
            hash.as_slice()
        ])? == 0
        {
            continue;
        }
        swept += 1;
        swept += store::discard_gapped_descendants(&tx, StreamId::try_from_sql(stream_id)?, &hash)?;
    }
    tx.commit()?;
    Ok(swept)
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
    table_sync_sweep_expired_gapped(conn, now_ms)?;
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
        let ctx = SyncCtx {
            repo_id: &repo_id,
            account_id,
            incarnation_ref,
            device: &device,
            registry: SYNCABLE_TABLES,
            now_ms,
            local_writer: Default::default(),
        };
        authored += author_repo_pending(conn, &ctx)?;
    }
    Ok(authored)
}

/// Own the rollback boundary so failed authoring cannot discard its diagnostic explanation.
pub(super) fn author_repo_pending(conn: &Connection, ctx: &SyncCtx<'_>) -> anyhow::Result<usize> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    match author_repo_in_tx(&tx, ctx) {
        Ok(authored) => {
            tx.commit()?;
            Ok(authored)
        },
        Err(error) => {
            tx.rollback()?;
            let observations = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            diagnostics::refresh_after_rollback(&observations, ctx)?;
            if let Some(conflict) = error.downcast_ref::<diagnostics::SelfApplyConflict>() {
                conflict.record_if_unexplained(&observations)?;
            }
            observations.commit()?;
            Err(error)
        },
    }
}

fn author_repo_in_tx(tx: &Transaction<'_>, ctx: &SyncCtx<'_>) -> anyhow::Result<usize> {
    crate::account::require_supported_account_control(tx, ctx.account_id)?;
    let mut authored = 0;
    // INVARIANT: the account fold enqueues re-adoption work for EVERY stream in
    // table_sync_streams, while this drain only walks repo-scoped specs' streams. The two
    // sets coincide as long as every registered production table is repo-scoped — registering
    // an account-scoped table must derive the drain set from the work table instead.
    let mut streams: Vec<crate::stream::StreamId> = ctx
        .registry
        .iter()
        .filter(|spec| spec.repo_column.is_some())
        .map(|spec| {
            scope_stream_id(ctx.repo_id, ctx.account_id, ctx.incarnation_ref, spec.scope_id)
        })
        .collect();
    let produced = engine::produce_and_author(tx, ctx)?;
    authored += produced.len();
    streams.sort_unstable();
    streams.dedup();
    for stream in streams {
        if coverage::stream_pending(tx, stream)? {
            continue;
        }
        // Drain EVERY pending removal, not one: two devices removed on one stream must not
        // wait a whole sync session for the second repair. Each call completes one removal, so
        // `has_pending` makes progress — UNLESS the pass cannot drain: a row whose synced
        // column is unreadable today (retried once the cell is repaired), or a stream with no
        // recorded apply context. `None` is that case: stop rather than spin on a work item
        // this pass cannot finish.
        while store::has_pending_readoption_work(tx, ctx.account_id, stream)? {
            let Some(reauthored) = engine::process_readoption_work_for_stream(tx, ctx, stream)?
            else {
                break;
            };
            authored += reauthored;
        }
    }
    Ok(authored)
}

/// The per-(stream, device) accepted-entry budget for a scope: `None` = full retention, `Some(n)` =
/// keep the newest `n` reclaimable entries and compact the rest. This is the production
/// `retain` argument to [`table_sync_compact_overdue`].
///
/// `anchors/1` is fully retained (durable memory anchors — every entry is history worth keeping).
/// `overlay/1` is the first bounded scope: the regeneration churn of its summary and verdict rows
/// would grow a chain without limit, so each device chain keeps [`OVERLAY_ACCEPTED_RETENTION`]
/// accepted entries. An unknown scope defaults to full retention.
pub const OVERLAY_ACCEPTED_RETENTION: u64 = 256;

/// `distill/1` is higher-churn than overlay: one record regeneration authors a burst of entries
/// across the cluster, so the budget must clear several full-cluster regenerations before the
/// retained floor advances. A large initial backfill is all live rows, which pin their chain over
/// budget until later regenerations supersede them — retention never drops a row a peer still
/// needs (#1277).
pub const DISTILL_ACCEPTED_RETENTION: u64 = 512;

pub fn scope_retention_budget(scope_id: &str) -> Option<u64> {
    match ScopeId::from_db_str(scope_id) {
        Some(ScopeId::OVERLAY) => Some(OVERLAY_ACCEPTED_RETENTION),
        Some(ScopeId::DISTILL) => Some(DISTILL_ACCEPTED_RETENTION),
        _ => None,
    }
}

/// The most entries one compaction pass authors on a chain to carry its pins forward: bounds the
/// pass's signing and transfer cost. A longer paying run moves over later passes.
const COMPACTION_REAUTHOR_MAX: usize = 64;

/// Compact accepted chain prefixes for scopes whose retention policy bounds them (#1127).
///
/// `retain` answers the per-chain accepted-entry budget for a scope, `None` for full retention
/// (anchors/1 stays unbounded). A chain with more RECLAIMABLE entries than its budget compacts
/// toward keeping its newest; chains within budget are untouched. Entries retained for
/// forward-compat replay (`pending_reason IS NOT NULL`) are neither counted nor dropped.
///
/// Only superseded entries drop (#1277, see the retention module docs): the floor clamps at the
/// chain's oldest pin. On this device's own chain the driver first carries forward,
/// unconditionally, every pin left below its floor by a store compacted under an older rule, then
/// carries the oldest pins above the floor to the tail — the longest run whose move frees at least
/// twice the entries it authors (live rows re-authored one each, stated deletes packed into
/// `Restate` batches), at most [`COMPACTION_REAUTHOR_MAX`] entries per pass. A foreign chain is
/// never re-authored. A floor that does not advance is a steady-state no-op, never an error, so the
/// driver is safely re-runnable on any cadence. Each stream compacts in its own IMMEDIATE
/// transaction.
///
/// Returns the number of accepted entries reclaimed.
pub fn table_sync_compact_overdue(
    conn: &Connection,
    account_id: AccountId,
    now_ms: i64,
    retain: &dyn Fn(&str) -> Option<u64>,
) -> anyhow::Result<usize> {
    compact_overdue_against(conn, account_id, now_ms, retain, SYNCABLE_TABLES)
}

fn compact_overdue_against(
    conn: &Connection,
    account_id: AccountId,
    now_ms: i64,
    retain: &dyn Fn(&str) -> Option<u64>,
    registry: &[TableSpec],
) -> anyhow::Result<usize> {
    let streams = supported_streams_against(conn, account_id, registry)?;
    let device = crate::local_device(conn, now_ms)?;
    let local = device.fingerprint();
    let writer = account::device_is_effective_writer(conn, account_id, local)?;
    let _durability = writer.then(|| crate::AuthoredDurability::begin(conn)).transpose()?;
    let mut compacted = 0;
    for stream in streams {
        let Some(keep) = retain(&stream.scope_id) else {
            continue;
        };
        anyhow::ensure!(keep >= 1, "a retention budget of zero keeps no chain tail to build on");
        let stream_id = StreamId::from_bytes(stream.stream_id);
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        crate::account::require_supported_account_control(&tx, account_id)?;
        if coverage::stream_pending(&tx, stream_id)? {
            continue;
        }
        let ctx = writer.then(|| SyncCtx {
            repo_id: &stream.repo_id,
            account_id,
            incarnation_ref: stream.incarnation_ref,
            device: &device,
            registry,
            now_ms,
            local_writer: Default::default(),
        });
        // Mandatory repair first. A store compacted under an older rule may have dropped the
        // entries carrying live rows or stating deletes below its floor: peers that folded only
        // the retained suffix never received them. Carrying every below-floor pin to the tail
        // repairs that once — the whole list in one call, so stated deletes pack together — and
        // afterwards nothing pins below the floor. A pin that cannot be carried today holds back
        // no other (`past_stuck`), and the economics never apply here.
        if let Some(ctx) = &ctx
            && let Some(floor) = retention::retained_floor(&tx, stream_id, local)?
        {
            let below = retention::chain_pins(&tx, stream_id, local, 0, floor, usize::MAX)?;
            engine::reauthor_chain_pins(
                &tx,
                ctx,
                &stream.scope_id,
                stream_id,
                &below,
                usize::MAX,
                true,
            )?;
        }
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
            let chain = crate::op::DeviceFingerprint::try_from_sql(device_bytes)?;
            // The budget floor is the oldest RECLAIMABLE entry to retain: the one at offset
            // (count - keep) in lamport order. compact_chain_prefix drops strictly below it.
            let target: i64 = tx.query_row(
                "SELECT lamport FROM table_sync_entries
                 WHERE stream_id = ?1 AND device_fingerprint = ?2 AND pending_reason IS NULL
                 ORDER BY lamport LIMIT 1 OFFSET ?3",
                params![
                    stream.stream_id.as_slice(),
                    chain.to_bytes().as_slice(),
                    i64::try_from(count - keep)?
                ],
                |row| row.get(0),
            )?;
            let target = u64::try_from(target)?;
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
                    params![stream.stream_id.as_slice(), chain.to_bytes().as_slice()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let target = match lowest_pending {
                Some(pending) if (pending as u64) < target => pending as u64,
                _ => target,
            };
            let from = retention::retained_floor(&tx, stream_id, chain)?.unwrap_or(0);
            if target <= from {
                continue;
            }
            // The writer's own chain weighs its whole run of pins; any other needs only the oldest.
            let reauthor = ctx.as_ref().filter(|_| chain == local);
            let limit = if reauthor.is_some() { usize::MAX } else { 1 };
            let pins = retention::chain_pins(&tx, stream_id, chain, from, target, limit)?;
            let floor = match (reauthor, pins.first()) {
                (_, None) => target,
                (Some(ctx), Some(_)) => {
                    let scoped = repo_registry(registry)
                        .into_iter()
                        .filter(|spec| spec.scope_id.as_db_str() == stream.scope_id)
                        .collect::<Vec<_>>();
                    let worth = pins_worth_reauthoring(
                        &tx,
                        &stream.repo_id,
                        stream_id,
                        chain,
                        &pins,
                        target,
                        &scoped,
                        COMPACTION_REAUTHOR_MAX,
                    )?;
                    let moved = engine::reauthor_chain_pins(
                        &tx,
                        ctx,
                        &stream.scope_id,
                        stream_id,
                        &pins[..worth],
                        COMPACTION_REAUTHOR_MAX,
                        false,
                    )?;
                    pins.get(moved).map_or(target, |pin| pin.lamport)
                },
                (None, Some(oldest)) => oldest.lamport,
            };
            // A chain pinned at its bottom (or already compacted past this point) computes a
            // floor that does not advance: steady state, not the caller bug compact_chain_prefix's
            // refusal exists to catch.
            if floor <= from {
                continue;
            }
            compacted += retention::compact_chain_prefix(&tx, stream_id, chain, floor, now_ms)?
                .dropped_entries;
        }
        tx.commit()?;
    }
    Ok(compacted)
}

/// How many of the chain's oldest `pins` are worth carrying forward: the largest `k` whose move
/// frees at least twice the entries it authors, so every entry authored reclaims at least one
/// more. Moving the first `k` pins authors `live(k)` upserts plus the `Restate` batches the stated
/// deletes among them pack into — per table, in pin order, regardless of interleaved live pins —
/// and frees every reclaimable entry below the next pin. Zero when no prefix pays for itself: a
/// chain that is all statements already packed at the tail authors nothing, however far over
/// budget, and reports its irreducible footprint by leaving the floor where it is.
///
/// Weighed over the whole run, not the per-pass cap: a paying move longer than the cap proceeds
/// over several passes, each one freeing at least the entries it authors. Weighing only the capped
/// prefix would stall for good behind more cold pins than the cap. The run this pass takes is
/// then the longest prefix of the paying run whose cost fits `cap`, so the pass never spends its
/// entries on pins it cannot finish. A pin whose table this binary does not register, whose key
/// does not fit an entry alone, or whose stored coordinates do not parse ends the run — nothing
/// past it can move this pass, and the authoring side leaves such a pin standing the same way.
#[expect(clippy::too_many_arguments, reason = "one estimate over one compaction pass's state")]
pub(crate) fn pins_worth_reauthoring(
    tx: &Transaction<'_>,
    repo_id: &str,
    stream: StreamId,
    chain: crate::op::DeviceFingerprint,
    pins: &[retention::Pin],
    target: u64,
    scoped: &[TableSpec],
    cap: usize,
) -> anyhow::Result<usize> {
    // Everything below `target` is reclaimable (the target clamps at the lowest pending entry).
    let lamports = tx
        .prepare(
            "SELECT lamport FROM table_sync_entries
             WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport < ?3
             ORDER BY lamport",
        )?
        .query_map(
            params![
                stream.to_bytes().as_slice(),
                chain.to_bytes().as_slice(),
                i64::try_from(target)?
            ],
            |row| row.get::<_, i64>(0),
        )?
        .map(|lamport| Ok(u64::try_from(lamport?)?))
        .collect::<anyhow::Result<Vec<u64>>>()?;
    let payload_max = engine::restate_payload_max();
    let mut packers: Vec<(&str, super::row_op::RestatePacker)> = Vec::new();
    // Rows this chain restated move as `RestateRows` batches, priced by batch like deletes.
    let mut row_packers: Vec<(&str, super::row_op::RestatePacker)> = Vec::new();
    let local_hex = chain.to_string();
    let mut live = 0;
    let mut worth = 0;
    let mut within_cap = 0;
    'run: for (index, pin) in pins.iter().enumerate() {
        match &pin.kind {
            retention::PinKind::LiveRow { table_name, row_pk } => {
                let Some(spec) = scoped.iter().find(|spec| spec.name == *table_name) else {
                    break 'run;
                };
                match engine::own_pin_move(tx, repo_id, spec, stream, row_pk, &local_hex)? {
                    engine::OwnPinMove::Upsert => live += 1,
                    engine::OwnPinMove::Stuck => break 'run,
                    engine::OwnPinMove::Restate(row) => {
                        let packer =
                            match row_packers.iter_mut().find(|(table, _)| *table == spec.name) {
                                Some((_, packer)) => packer,
                                None => {
                                    row_packers.push((
                                        spec.name,
                                        super::row_op::RestatePacker::for_rows(
                                            spec.name,
                                            spec.spec_version,
                                            payload_max,
                                        ),
                                    ));
                                    &mut row_packers.last_mut().expect("just pushed").1
                                },
                            };
                        // Too large to restate alone: it moves as a tail upsert instead.
                        if packer.push_row(&row) == super::row_op::Placed::Unfit {
                            live += 1;
                        }
                    },
                }
            },
            retention::PinKind::Statements(rows) =>
                for row in rows {
                    let Some(spec) = scoped.iter().find(|spec| spec.name == row.table_name) else {
                        break 'run;
                    };
                    let (Ok(pk), Ok(device)) = (
                        super::row_op::row_pk_values(&row.row_pk),
                        row.device_hex.parse::<crate::op::DeviceFingerprint>(),
                    ) else {
                        break 'run;
                    };
                    let delete = super::row_op::StatedDelete { pk, device, lamport: row.lamport };
                    let packer = match packers.iter_mut().find(|(table, _)| *table == spec.name) {
                        Some((_, packer)) => packer,
                        None => {
                            packers.push((
                                spec.name,
                                super::row_op::RestatePacker::new(
                                    spec.name,
                                    spec.spec_version,
                                    payload_max,
                                ),
                            ));
                            &mut packers.last_mut().expect("just pushed").1
                        },
                    };
                    if packer.push(&delete) == super::row_op::Placed::Unfit {
                        break 'run;
                    }
                },
        }
        let k = index + 1;
        let cost = live
            + packers.iter().chain(&row_packers).map(|(_, packer)| packer.batches()).sum::<usize>();
        if cost <= cap {
            within_cap = k;
        }
        let bound = pins.get(k).map_or(target, |pin| pin.lamport);
        if lamports.partition_point(|&lamport| lamport < bound) >= 2 * cost {
            worth = k;
        }
        // Cost only grows along the run, and nothing past `lamports` can be reclaimed: once the
        // cost passes the cap or half of everything reclaimable, no longer prefix can qualify, so
        // stop before pricing more pins (each reads its row).
        if cost > cap || 2 * cost > lamports.len() {
            break;
        }
    }
    Ok(worth.min(within_cap))
}

/// Recompute an advertised route from local current-incarnation authority and the production
/// registry. The advertised stream id is routing advice only; it never creates authority.
pub fn table_sync_validate_stream(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
) -> anyhow::Result<bool> {
    let _snapshot = crate::account::control_policy::read_snapshot(conn)?;
    crate::account::require_supported_account_control(conn, account_id)?;
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
    let _snapshot = crate::account::control_policy::read_snapshot(conn)?;
    crate::account::require_supported_account_control(conn, account_id)?;
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
    let _snapshot = crate::account::control_policy::read_snapshot(conn)?;
    crate::account::require_supported_account_control(conn, account_id)?;
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
    let _snapshot = crate::account::control_policy::read_snapshot(conn)?;
    crate::account::require_supported_account_control(conn, account_id)?;
    if limit == 0 || !table_sync_validate_stream(conn, account_id, stream)? {
        return Ok(Vec::new());
    }
    accepted_chain_entries(conn, stream.stream_id, device_fingerprint, start, limit)
}

/// Whether this current stream still owes an advertised suffix. This is delivery progress,
/// not a statement that every retained entry projects or that every peer has synchronized.
pub fn table_sync_has_pending_coverage(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
) -> anyhow::Result<bool> {
    let _snapshot = crate::account::control_policy::read_snapshot(conn)?;
    crate::account::require_supported_account_control(conn, account_id)?;
    Ok(table_sync_validate_stream(conn, account_id, stream)?
        && coverage::stream_pending(conn, StreamId::from_bytes(stream.stream_id))?)
}

/// One received table-sync entry as the session hands it to [`table_sync_ingest`]: the chain it
/// was paged from, its bytes, and the floor the peer advertised for that chain.
pub struct TableSyncReceived<'a> {
    pub expected_device: [u8; 32],
    pub signed_bytes: &'a [u8],
    pub advertised_floor: Option<TableSyncChainCursor>,
    /// The offered tip is routing advice, never an accepted chain witness.
    pub advertised_tip: Option<TableSyncChainCursor>,
}

/// Feed one untrusted signed envelope through the existing table-sync authority, chain and payload
/// gates. Invalid/stale routes are skipped before a transaction can write any table-sync state.
pub fn table_sync_ingest(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    received: &TableSyncReceived<'_>,
    now_ms: i64,
    local_writer: &LocalWriterMemo,
) -> anyhow::Result<TableSyncIngestOutcome> {
    ingest_received_against(
        conn,
        &IngestRoute { account_id, stream, registry: SYNCABLE_TABLES },
        received,
        now_ms,
        local_writer,
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
    let scopes: BTreeSet<ScopeId> = registry
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
        let incarnation_ref = cbor::sql_fixed(incarnation, "repository incarnation")?;
        for &scope_id in &scopes {
            streams.push(TableSyncStream {
                stream_id: scope_stream_id(&repo_id, account_id, incarnation_ref, scope_id)
                    .to_bytes(),
                repo_id: repo_id.clone(),
                incarnation_ref,
                scope_id: scope_id.as_db_str().to_string(),
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
    Ok(validated_scope(conn, account_id, stream, registry)?.is_some())
}

/// The registered scope an advertised route resolves to, or `None` when the route is not current:
/// its scope names no repo-scoped spec in `registry`, its incarnation is not the account's current
/// one, or its stream id does not re-derive from the rest.
fn validated_scope(
    conn: &Connection,
    account_id: AccountId,
    stream: &TableSyncStream,
    registry: &[TableSpec],
) -> anyhow::Result<Option<ScopeId>> {
    let Some(scope) = registry
        .iter()
        .filter(|spec| spec.repo_column.is_some())
        .map(|spec| spec.scope_id)
        .find(|scope| scope.as_db_str() == stream.scope_id)
    else {
        return Ok(None);
    };
    let current = account::repo_incarnation_state(conn, account_id, &stream.repo_id)?;
    if current
        != RepoIncarnationState::Current(crate::AccountEntryHash::from_bytes(
            stream.incarnation_ref,
        ))
    {
        return Ok(None);
    }
    let derived = scope_stream_id(&stream.repo_id, account_id, stream.incarnation_ref, scope);
    Ok((derived.to_bytes() == stream.stream_id).then_some(scope))
}

fn accepted_chain_page(
    conn: &Connection,
    stream_id: [u8; 32],
    after_device: Option<[u8; 32]>,
    limit: usize,
) -> anyhow::Result<Vec<TableSyncChainHead>> {
    let mut stmt = conn.prepare(
        "SELECT e.device_fingerprint, COALESCE(c.tip_lamport, e.lamport),
                COALESCE(c.tip_hash, e.entry_hash), f.lamport, f.entry_hash
           FROM table_sync_entries e
           LEFT JOIN table_sync_retained_floors f
             ON f.stream_id = e.stream_id
            AND f.device_fingerprint = e.device_fingerprint
           LEFT JOIN table_sync_suffix_coverage c
             ON c.stream_id = e.stream_id AND c.device_fingerprint = e.device_fingerprint
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
                Ok(TableSyncChainCursor {
                    lamport: u64::try_from(l)?,
                    entry_hash: cbor::sql_fixed(
                        floor_hash.expect("floor row carries its entry hash"),
                        "entry_hash",
                    )?,
                })
            })
            .transpose()?;
        Ok(TableSyncChainHead {
            device_fingerprint: cbor::sql_fixed(device, "device_fingerprint")?,
            lamport: u64::try_from(lamport)?,
            entry_hash: cbor::sql_fixed(hash, "entry_hash")?,
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
        .map(|(lamport, hash)| TableSyncChainCursor::from_row(lamport, hash))
        .transpose()?;
    match (accepted, witness) {
        (None, None) => Ok(TableSyncFrontier::Empty),
        (Some(accepted), None) => Ok(TableSyncFrontier::Accepted(accepted)),
        (None, Some(witness)) => Ok(TableSyncFrontier::Restore(witness)),
        (Some(accepted), Some(witness)) if accepted == witness =>
            Ok(TableSyncFrontier::Accepted(accepted)),
        (Some(accepted), Some(witness)) if witness.lamport > accepted.lamport =>
            Ok(TableSyncFrontier::Restore(witness)),
        // Rendered as `(lamport, hash)` tuples: the wording this error has always had.
        (Some(accepted), Some(witness)) => anyhow::bail!(
            "table-sync chain tip witness {:?} conflicts with accepted tail {:?}",
            (witness.lamport, witness.entry_hash),
            (accepted.lamport, accepted.entry_hash)
        ),
    }
}

fn accepted_chain_tail(
    conn: &Connection,
    stream_id: [u8; 32],
    device_fingerprint: [u8; 32],
) -> anyhow::Result<Option<TableSyncChainCursor>> {
    conn.query_row(
        "SELECT lamport, entry_hash FROM table_sync_entries
          WHERE stream_id = ?1 AND device_fingerprint = ?2
          ORDER BY lamport DESC LIMIT 1",
        params![stream_id.as_slice(), device_fingerprint.as_slice()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )
    .optional()?
    .map(|(lamport, hash)| TableSyncChainCursor::from_row(lamport, hash))
    .transpose()
}

fn accepted_chain_entries(
    conn: &Connection,
    stream_id: [u8; 32],
    device_fingerprint: [u8; 32],
    start: TableSyncEntryStart,
    limit: usize,
) -> anyhow::Result<Vec<TableSyncChainEntry>> {
    let stream = StreamId::from_bytes(stream_id);
    let device = crate::op::DeviceFingerprint::from_bytes(device_fingerprint);
    if let TableSyncEntryStart::After(cursor) | TableSyncEntryStart::At(cursor) = start
        && let Some(pending) = coverage::pending_tip(conn, stream, device)?
        && let Some(tail) = accepted_chain_tail(conn, stream_id, device_fingerprint)?
        && cursor.lamport > tail.lamport
        && cursor.lamport <= pending.lamport
    {
        anyhow::ensure!(
            cursor.lamport != pending.lamport || cursor.entry_hash == pending.entry_hash.to_bytes(),
            "table-sync requested cursor conflicts with the promised suffix tip"
        );
        // A peer is further through the same outstanding suffix. We cannot serve its cursor,
        // but an empty pending direction lets it supply our missing prefix in the reverse turn.
        return Ok(Vec::new());
    }
    let (minimum_lamport, inclusive) = match start {
        TableSyncEntryStart::Beginning => (None, false),
        TableSyncEntryStart::After(cursor) => {
            if !cursor_matches(conn, stream, device, cursor.to_store())? {
                // A chain restored after a repository purge starts at its witness: nothing below
                // it is held here, and the witness is not a floor this store may advertise (it was
                // never checked against the rows' carriers, and floors propagate). A peer whose
                // tip sits below it is honest; answer with nothing so the session survives and it
                // fills the prefix from a peer that still holds it (#1481).
                if below_rootless_holdings(conn, stream, device, cursor.lamport)? {
                    return Ok(Vec::new());
                }
                return Err(UnservableChainCursor::NotHeld.into());
            }
            (Some(cursor.lamport), false)
        },
        TableSyncEntryStart::At(cursor) => {
            if cursor_matches(conn, stream, device, cursor.to_store())? {
                (Some(cursor.lamport), true)
            } else {
                let successor = direct_successor_lamport(conn, stream, device, cursor.to_store())?;
                // The receiver was purge-restored at a witness below everything a purge-restored
                // chain holds here: the same honest shape as a tip below it (#1481).
                if successor.is_none()
                    && below_rootless_holdings(conn, stream, device, cursor.lamport)?
                {
                    return Ok(Vec::new());
                }
                let Some(successor) = successor else {
                    return Err(UnservableChainCursor::NoRestoreSuccessor.into());
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
            cursor: TableSyncChainCursor::from_row(lamport, hash)?,
            signed_bytes,
        })
    })
    .collect::<anyhow::Result<_>>()
}

/// A chain cursor a peer named that this store cannot serve from, although it holds that chain:
/// the peer's copy diverged from this one (a chain signed twice at one point, #1417), or it names
/// a point below a chain this store holds from its first entry. Retrying never serves it, so a
/// session skips that one chain rather than failing (#1480). A cursor below a purge-restored or
/// compacted chain's holdings is not this error: it gets an empty page and a later re-plan.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UnservableChainCursor {
    #[error("table-sync accepted chain cursor is not present locally")]
    NotHeld,
    #[error("table-sync chain cursor hash conflicts at lamport {0}")]
    HashConflict(u64),
    #[error("table-sync restore cursor has neither its tip nor a direct successor")]
    NoRestoreSuccessor,
}

/// Whether `lamport` sits below everything this store holds of the chain, and the lowest held
/// entry has a predecessor — which is then not held here: the chain was restored at a witness
/// after a purge, or re-rooted past compacted history, rather than held from its first entry. A
/// chain held from its first entry holds everything its signer put below its tip, so a cursor
/// below that is no honest shape and stays an error.
fn below_rootless_holdings(
    conn: &Connection,
    stream: StreamId,
    device: crate::op::DeviceFingerprint,
    lamport: u64,
) -> anyhow::Result<bool> {
    let lowest: Option<(i64, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT lamport, prev_hash FROM table_sync_entries
              WHERE stream_id = ?1 AND device_fingerprint = ?2
              ORDER BY lamport LIMIT 1",
            params![stream.to_bytes().as_slice(), device.to_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((lowest, Some(_))) = lowest else { return Ok(false) };
    Ok(lamport < u64::try_from(lowest)?)
}

fn direct_successor_lamport(
    conn: &Connection,
    stream: StreamId,
    device: crate::op::DeviceFingerprint,
    witness: store::ChainCursor,
) -> anyhow::Result<Option<u64>> {
    conn.query_row(
        "SELECT lamport FROM table_sync_entries
          WHERE stream_id = ?1 AND device_fingerprint = ?2 AND prev_hash = ?3 AND lamport > ?4
          ORDER BY lamport LIMIT 1",
        params![
            stream.to_bytes().as_slice(),
            device.to_bytes().as_slice(),
            witness.entry_hash.as_slice(),
            i64::try_from(witness.lamport)?,
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
    stream: StreamId,
    device: crate::op::DeviceFingerprint,
    cursor: store::ChainCursor,
) -> anyhow::Result<bool> {
    let stored = conn
        .query_row(
            "SELECT entry_hash FROM table_sync_entries
              WHERE stream_id = ?1 AND device_fingerprint = ?2 AND lamport = ?3",
            params![
                stream.to_bytes().as_slice(),
                device.to_bytes().as_slice(),
                i64::try_from(cursor.lamport)?,
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    let Some(stored) = stored else { return Ok(false) };
    if EntryHash::try_from_sql(stored)? != cursor.entry_hash {
        return Err(UnservableChainCursor::HashConflict(cursor.lamport).into());
    }
    Ok(true)
}

/// The route one ingest is validated against: the account, the stream it arrived on, and the
/// registry that decides which tables that stream may carry.
#[derive(Clone, Copy)]
struct IngestRoute<'a> {
    account_id: AccountId,
    stream: &'a TableSyncStream,
    registry: &'a [TableSpec],
}

#[cfg(test)]
fn ingest_against(
    conn: &Connection,
    route: &IngestRoute<'_>,
    expected_device: crate::op::DeviceFingerprint,
    signed_bytes: &[u8],
    now_ms: i64,
    advertised_floor: Option<TableSyncChainCursor>,
    local_writer: &LocalWriterMemo,
) -> anyhow::Result<TableSyncIngestOutcome> {
    ingest_received_against(
        conn,
        route,
        &TableSyncReceived {
            expected_device: expected_device.to_bytes(),
            signed_bytes,
            advertised_floor,
            // Legacy unit fixtures offer a single floor entry.
            advertised_tip: advertised_floor,
        },
        now_ms,
        local_writer,
    )
}

fn ingest_received_against(
    conn: &Connection,
    route: &IngestRoute<'_>,
    received: &TableSyncReceived<'_>,
    now_ms: i64,
    local_writer: &LocalWriterMemo,
) -> anyhow::Result<TableSyncIngestOutcome> {
    let TableSyncReceived { expected_device, signed_bytes, advertised_floor, advertised_tip } =
        *received;
    let expected_device = crate::op::DeviceFingerprint::from_bytes(expected_device);
    let IngestRoute { account_id, stream, registry } = *route;
    let Some(scope) = validated_scope(conn, account_id, stream, registry)? else {
        return Ok(TableSyncIngestOutcome::NoChange);
    };
    let Ok(signed) = crate::entry::decode_signed(signed_bytes) else {
        return Ok(TableSyncIngestOutcome::NoChange);
    };
    if signed.entry.stream_id.to_bytes() != stream.stream_id {
        return Ok(TableSyncIngestOutcome::NoChange);
    }
    let signer = signed.entry.device_fingerprint;
    if signer != expected_device {
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
    crate::account::require_supported_account_control(&tx, account_id)?;
    let ctx = SyncCtx {
        repo_id: &stream.repo_id,
        account_id,
        incarnation_ref: stream.incarnation_ref,
        device: &local,
        registry: &registry,
        now_ms,
        local_writer: local_writer.clone(),
    };
    let stream_id = StreamId::from_bytes(stream.stream_id);
    let floor_before = retention::retained_floor(&tx, stream_id, signer)?;
    // A newer floor re-roots a store that still owes an earlier root's suffix exactly as it
    // re-roots one that never heard of that suffix: the floor carries its own contract (every
    // current carrier on the chain is at or above it at the advertising store), and the obligation
    // then follows the new root (#1489). Holding out for the old tip instead wedged the stream for
    // good once every holder had compacted that tip away. A caller without the inventory tip may
    // still offer ordinary contiguous entries, but cannot create a floor whose required suffix
    // would be forgotten immediately.
    let usable_floor = advertised_floor.filter(|_| advertised_tip.is_some());
    if let (Some(floor), Some(tip)) = (usable_floor, advertised_tip) {
        anyhow::ensure!(
            tip.lamport >= floor.lamport && tip.lamport < crate::entry::MAX_ENTRY_LAMPORT,
            "invalid advertised table suffix tip"
        );
        anyhow::ensure!(
            tip.lamport != floor.lamport || tip.entry_hash == floor.entry_hash,
            "advertised floor and tip conflict at the same lamport"
        );
    }
    let report = engine::ingest(
        &tx,
        &ctx,
        scope,
        signed_bytes,
        &pubkey,
        usable_floor.map(TableSyncChainCursor::to_store),
    )?;
    let floor_after = retention::retained_floor(&tx, stream_id, signer)?;
    if floor_after != floor_before
        && let (Some(floor), Some(tip)) = (floor_after, advertised_tip)
    {
        coverage::record(&tx, stream_id, signer, floor, tip.to_store())?;
    }
    coverage::clear_delivered(&tx, stream_id, signer)?;
    // An applied row on this stream changed derived state, so advance the Lens lanes that scope
    // feeds (the explicit replacement for the row triggers the synced scopes dropped). All entries
    // in one ingest belong to one stream, hence one scope, so `scope_lens_metas(stream.scope_id)`
    // is the exact lane set even though `IngestOutcome` does not name the applied table. The
    // registered-repo gate matches the dropped triggers and avoids phantom `'__unassigned__'` rows.
    let lens_metas = scope_lens_metas(&stream.scope_id);
    if !lens_metas.is_empty()
        && std::iter::once(&report.outcome)
            .chain(report.promoted.iter())
            .any(|outcome| matches!(outcome, IngestOutcome::Applied))
        && rag_rat_db::schema::repo_id_is_registered(&tx, &stream.repo_id)?
    {
        rag_rat_db::meta::bump_lens_revisions(&tx, &stream.repo_id, lens_metas)?;
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
#[path = "transport_tests.rs"]
mod tests;
