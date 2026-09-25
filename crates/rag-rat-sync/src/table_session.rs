//! Authenticated multi-stream table reconciliation over one bidirectional stream.

use std::collections::HashSet;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::auth::{self, AuthRole, PeerCapability, SessionCapabilities};
use crate::session::{DEFAULT_IDLE_TIMEOUT, Ingested, MAX_SESSION_ENTRIES};
use crate::table_codec::{self, TableCodecError};
use crate::table_wire::{
    ChainFrontier, ChainHead, FrontierState, MAX_TABLE_CHAINS_PER_PAGE,
    MAX_TABLE_CHAINS_PER_SESSION, MAX_TABLE_ENTRIES_PER_PAGE, MAX_TABLE_ENTRY_BYTES, Manifest,
    ManifestItem, TableFrame,
};

type Hash = [u8; 32];

/// Store operations needed by the stream-qualified table session.
pub trait TableSyncStore {
    fn account_id(&self) -> Hash;
    fn prepare(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn has_pending_coverage(&self, _item: &ManifestItem) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn supported_streams(&self) -> anyhow::Result<Vec<ManifestItem>>;
    fn validates(&self, item: &ManifestItem) -> anyhow::Result<bool>;
    fn chain_page(
        &self,
        item: &ManifestItem,
        after_device: Option<Hash>,
        limit: usize,
    ) -> anyhow::Result<Vec<ChainHead>>;
    fn frontier(&self, item: &ManifestItem, device: Hash) -> anyhow::Result<FrontierState>;
    fn entries(
        &self,
        item: &ManifestItem,
        device: Hash,
        start: ChainStart,
        limit: usize,
    ) -> anyhow::Result<Vec<ChainEntry>>;
    fn ingest(
        &mut self,
        item: &ManifestItem,
        offered: &ChainHead,
        signed_bytes: &[u8],
    ) -> anyhow::Result<Ingested>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainStart {
    Beginning,
    After { lamport: u64, entry_hash: Hash },
    At { lamport: u64, entry_hash: Hash },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEntry {
    pub lamport: u64,
    pub entry_hash: Hash,
    pub signed_bytes: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TableSessionReport {
    pub streams: usize,
    pub entries_sent: usize,
    pub entries_received: usize,
    pub entries_newly_stored: usize,
    pub continuation_pending: bool,
    /// Chains this side skipped as a sender because the peer's copy diverged from its own (#1480).
    pub chains_skipped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum TableSessionError {
    #[error("table-sync session transport: {0}")]
    Codec(#[from] TableCodecError),
    #[error("table-sync protocol violation: {0}")]
    Protocol(String),
    /// The peer made no progress within the idle window: it sent no frame, took none of ours, or
    /// did not let the stream close. Distinct from [`TableSessionError::Protocol`] — a silent peer
    /// violated nothing.
    #[error("table-sync peer made no progress within {after:?}")]
    Timeout { after: Duration },
    #[error("read-only peer attempted to push table entries")]
    UnauthorizedPush,
    #[error("table-sync session store: {0}")]
    Store(anyhow::Error),
}

pub async fn run_table_session<S, R, W>(
    store: &mut S,
    send: W,
    recv: R,
    role: AuthRole,
    capabilities: SessionCapabilities,
) -> Result<TableSessionReport, TableSessionError>
where
    S: TableSyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    run_table_session_with_limits(
        store,
        send,
        recv,
        role,
        capabilities,
        TableSessionLimits::default(),
    )
    .await
}

/// The per-session bounds of [`run_table_session_with_limits`]. The default is the session
/// [`run_table_session`] runs: the default idle timeout and the protocol's page and session caps.
#[derive(Clone, Copy)]
struct TableSessionLimits {
    /// Every frame read or write fails if the peer makes no progress within this window.
    idle_timeout: Duration,
    chains_per_page: usize,
    chains_per_session: usize,
    entries_per_page: usize,
    entries_per_session: usize,
}

impl Default for TableSessionLimits {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            chains_per_page: MAX_TABLE_CHAINS_PER_PAGE,
            chains_per_session: MAX_TABLE_CHAINS_PER_SESSION,
            entries_per_page: MAX_TABLE_ENTRIES_PER_PAGE,
            entries_per_session: MAX_SESSION_ENTRIES,
        }
    }
}

async fn run_table_session_with_limits<S, R, W>(
    store: &mut S,
    mut send: W,
    mut recv: R,
    role: AuthRole,
    capabilities: SessionCapabilities,
    limits: TableSessionLimits,
) -> Result<TableSessionReport, TableSessionError>
where
    S: TableSyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let idle_timeout = limits.idle_timeout;
    debug_assert!(limits.chains_per_page > 0);
    debug_assert!(limits.chains_per_page <= limits.chains_per_session);
    debug_assert!(limits.entries_per_page > 0);
    debug_assert!(limits.entries_per_page <= MAX_TABLE_ENTRIES_PER_PAGE);
    if capabilities.local.can_push() {
        store.prepare().map_err(TableSessionError::Store)?;
    }
    let local_manifest =
        Manifest::new(store.supported_streams().map_err(TableSessionError::Store)?)
            .map_err(|error| TableSessionError::Store(error.into()))?;
    let local_routes: HashSet<ManifestItem> = local_manifest.items().iter().cloned().collect();
    let manifest_frame = TableFrame::Manifest(local_manifest);
    let send_manifest = write_before(&mut send, &manifest_frame, idle_timeout);
    let receive_manifest = async {
        let TableFrame::Manifest(manifest) = read_before(&mut recv, idle_timeout).await? else {
            return Err(TableSessionError::Protocol("peer did not open with a manifest".into()));
        };
        Ok::<_, TableSessionError>(manifest)
    };
    let ((), peer_manifest) = tokio::try_join!(send_manifest, receive_manifest)?;

    let mut intersection = Vec::new();
    for item in peer_manifest.items() {
        if local_routes.contains(item) && store.validates(item).map_err(TableSessionError::Store)? {
            intersection.push(item.clone());
        }
    }
    let streams = intersection.len();
    let (sent, entries_received, entries_newly_stored, peer_pending) = match role {
        AuthRole::Dialer => {
            let sent = send_direction(
                store,
                &intersection,
                &mut send,
                &mut recv,
                capabilities.local,
                limits,
            )
            .await?;
            let (entries_received, entries_newly_stored, peer_pending) = receive_direction(
                store,
                &intersection,
                &mut send,
                &mut recv,
                capabilities.peer,
                limits,
            )
            .await?;
            (sent, entries_received, entries_newly_stored, peer_pending)
        },
        AuthRole::Acceptor => {
            let (entries_received, entries_newly_stored, peer_pending) = receive_direction(
                store,
                &intersection,
                &mut send,
                &mut recv,
                capabilities.peer,
                limits,
            )
            .await?;
            let sent = send_direction(
                store,
                &intersection,
                &mut send,
                &mut recv,
                capabilities.local,
                limits,
            )
            .await?;
            (sent, entries_received, entries_newly_stored, peer_pending)
        },
    };
    role.acknowledge_in_order(send_ack(&mut send, idle_timeout), read_ack(&mut recv, idle_timeout))
        .await?;
    Ok(TableSessionReport {
        streams,
        entries_sent: sent.entries,
        entries_received,
        entries_newly_stored,
        continuation_pending: sent.pending || peer_pending,
        chains_skipped: sent.chains_skipped,
    })
}

/// What one send direction moved.
#[derive(Debug, PartialEq, Eq)]
struct Sent {
    entries: usize,
    pending: bool,
    chains_skipped: usize,
}

async fn send_direction<S, R, W>(
    store: &S,
    streams: &[ManifestItem],
    send: &mut W,
    recv: &mut R,
    local_capability: PeerCapability,
    limits: TableSessionLimits,
) -> Result<Sent, TableSessionError>
where
    S: TableSyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let TableSessionLimits { idle_timeout, .. } = limits;
    let mut sent = 0;
    let mut offered_chains: usize = 0;
    let mut continuation_pending = false;
    let mut chains_skipped = 0;
    for item in streams {
        let mut after_device = None;
        let mut stream_pending =
            store.has_pending_coverage(item).map_err(TableSessionError::Store)?;
        loop {
            if !local_capability.can_push() {
                break;
            }
            if sent >= limits.entries_per_session {
                stream_pending = true;
                break;
            }
            if offered_chains >= limits.chains_per_session {
                let has_more = !store
                    .chain_page(item, after_device, 1)
                    .map_err(TableSessionError::Store)?
                    .is_empty();
                if has_more {
                    return Err(TableSessionError::Store(anyhow::anyhow!(
                        "table-sync manifest intersection exceeds the {}-device chain ceiling",
                        limits.chains_per_session
                    )));
                }
                break;
            }
            let chain_limit = limits
                .chains_per_page
                .min(limits.chains_per_session.saturating_sub(offered_chains));
            let chains = store
                .chain_page(item, after_device, chain_limit)
                .map_err(TableSessionError::Store)?;
            if chains.is_empty() {
                break;
            }
            if chains.len() > chain_limit {
                return Err(TableSessionError::Store(anyhow::anyhow!(
                    "local table-sync chain page exceeds {} chains",
                    chain_limit
                )));
            }
            offered_chains += chains.len();
            validate_chain_page(&chains).map_err(TableSessionError::Store)?;
            write_before(
                send,
                &TableFrame::ChainInventory { stream_id: item.stream_id, chains: chains.clone() },
                idle_timeout,
            )
            .await?;
            let TableFrame::ChainFrontiers { stream_id, frontiers } =
                read_before(recv, idle_timeout).await?
            else {
                return Err(TableSessionError::Protocol(
                    "peer did not answer a table chain inventory".into(),
                ));
            };
            if stream_id != item.stream_id
                || frontiers.len() != chains.len()
                || !frontiers.iter().zip(&chains).all(|(frontier, chain)| {
                    frontier.device_fingerprint == chain.device_fingerprint
                })
            {
                return Err(TableSessionError::Protocol(
                    "peer chain frontiers do not match the offered inventory".into(),
                ));
            }

            for (chain, frontier) in chains.iter().zip(frontiers) {
                let skip = |chains_skipped: &mut usize| {
                    // The peer's copy of this chain diverged from ours (a chain signed twice at
                    // one point, #1417): retrying never serves it, and failing would abandon every
                    // other chain and stream with this peer on every pass (#1480). Skip it alone.
                    tracing::warn!(
                        stream = %rag_rat_base::hash::hex_lower(&item.stream_id),
                        device = %rag_rat_base::hash::hex_lower(&chain.device_fingerprint),
                        "table-sync peer's copy of a chain diverged from ours; skipping that chain"
                    );
                    *chains_skipped += 1;
                };
                let mut start = match chain_plan(chain, frontier.state)? {
                    ChainPlan::Complete => continue,
                    ChainPlan::Send(start) => start,
                    ChainPlan::Pending => {
                        stream_pending = true;
                        continue;
                    },
                    ChainPlan::Diverged => {
                        skip(&mut chains_skipped);
                        continue;
                    },
                };
                while sent < limits.entries_per_session {
                    let page_limit = limits
                        .entries_per_page
                        .min(limits.entries_per_session.saturating_sub(sent));
                    let entries =
                        match store.entries(item, chain.device_fingerprint, start, page_limit) {
                            Ok(entries) => entries,
                            Err(error)
                                if error
                                    .downcast_ref::<rag_rat_oplog::UnservableChainCursor>()
                                    .is_some() =>
                            {
                                skip(&mut chains_skipped);
                                break;
                            },
                            Err(error) => return Err(TableSessionError::Store(error)),
                        };
                    if entries.is_empty() {
                        let delivered = match start {
                            ChainStart::After { lamport, entry_hash } =>
                                lamport > chain.lamport
                                    || (lamport == chain.lamport && entry_hash == chain.entry_hash),
                            _ => false,
                        };
                        stream_pending |= !delivered;
                        break;
                    }
                    if entries.len() > page_limit
                        || entries
                            .iter()
                            .any(|entry| entry.signed_bytes.len() > MAX_TABLE_ENTRY_BYTES)
                    {
                        return Err(TableSessionError::Store(anyhow::anyhow!(
                            "local table-sync entry page exceeds its transport bound"
                        )));
                    }
                    let last = entries.last().expect("non-empty page checked above");
                    start =
                        ChainStart::After { lamport: last.lamport, entry_hash: last.entry_hash };
                    sent += entries.len();
                    write_before(
                        send,
                        &TableFrame::Entries {
                            stream_id: item.stream_id,
                            device_fingerprint: chain.device_fingerprint,
                            entries: entries.into_iter().map(|entry| entry.signed_bytes).collect(),
                        },
                        idle_timeout,
                    )
                    .await?;
                }
                if sent >= limits.entries_per_session {
                    break;
                }
            }
            write_before(
                send,
                &TableFrame::InventoryDone { stream_id: item.stream_id },
                idle_timeout,
            )
            .await?;
            if sent >= limits.entries_per_session {
                stream_pending = true;
                break;
            }
            after_device = chains.last().map(|chain| chain.device_fingerprint);
        }
        write_before(
            send,
            &TableFrame::StreamDone {
                stream_id: item.stream_id,
                continuation_pending: stream_pending,
            },
            idle_timeout,
        )
        .await?;
        continuation_pending |= stream_pending;
    }
    write_before(send, &TableFrame::Done, idle_timeout).await?;
    Ok(Sent { entries: sent, pending: continuation_pending, chains_skipped })
}

async fn receive_direction<S, R, W>(
    store: &mut S,
    streams: &[ManifestItem],
    send: &mut W,
    recv: &mut R,
    peer_capability: PeerCapability,
    limits: TableSessionLimits,
) -> Result<(usize, usize, bool), TableSessionError>
where
    S: TableSyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let TableSessionLimits { idle_timeout, .. } = limits;
    let mut received = 0;
    let mut newly_stored = 0;
    let mut offered_chains: usize = 0;
    let mut continuation_pending = false;
    for item in streams {
        let mut last_device = None;
        loop {
            match read_before(recv, idle_timeout).await? {
                TableFrame::ChainInventory { stream_id, chains } => {
                    if !peer_capability.can_push() {
                        return Err(TableSessionError::UnauthorizedPush);
                    }
                    let ordered_after_previous = chains.first().is_some_and(|first| {
                        last_device.is_none_or(|previous| first.device_fingerprint > previous)
                    });
                    if stream_id != item.stream_id
                        || chains.len() > limits.chains_per_page
                        || offered_chains.saturating_add(chains.len()) > limits.chains_per_session
                        || !ordered_after_previous
                    {
                        return Err(TableSessionError::Protocol(
                            "peer chain inventory names the wrong stream or exceeds the session \
                             cap"
                            .into(),
                        ));
                    }
                    offered_chains += chains.len();
                    last_device = chains.last().map(|chain| chain.device_fingerprint);
                    let frontiers = chains
                        .iter()
                        .map(|chain| {
                            store.frontier(item, chain.device_fingerprint).map(|state| {
                                ChainFrontier {
                                    device_fingerprint: chain.device_fingerprint,
                                    state,
                                }
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()
                        .map_err(TableSessionError::Store)?;
                    write_before(
                        send,
                        &TableFrame::ChainFrontiers { stream_id, frontiers },
                        idle_timeout,
                    )
                    .await?;
                    loop {
                        match read_before(recv, idle_timeout).await? {
                            TableFrame::Entries { stream_id, device_fingerprint, entries }
                                if stream_id == item.stream_id
                                    && chains.iter().any(|chain| {
                                        chain.device_fingerprint == device_fingerprint
                                    }) =>
                            {
                                received += entries.len();
                                if received > limits.entries_per_session {
                                    return Err(TableSessionError::Protocol(format!(
                                        "peer streamed more than {} table entries",
                                        limits.entries_per_session
                                    )));
                                }
                                let offered = chains
                                    .iter()
                                    .find(|chain| chain.device_fingerprint == device_fingerprint)
                                    .expect("entry chain belongs to this inventory");
                                for bytes in entries {
                                    if store
                                        .ingest(item, offered, &bytes)
                                        .map_err(TableSessionError::Store)?
                                        == Ingested::Stored
                                    {
                                        newly_stored += 1;
                                    }
                                }
                            },
                            TableFrame::InventoryDone { stream_id }
                                if stream_id == item.stream_id =>
                                break,
                            _ => {
                                return Err(TableSessionError::Protocol(
                                    "peer sent an out-of-sequence table inventory response".into(),
                                ));
                            },
                        }
                    }
                },
                TableFrame::StreamDone { stream_id, continuation_pending: pending }
                    if stream_id == item.stream_id =>
                {
                    continuation_pending |= pending;
                    break;
                },
                _ => {
                    return Err(TableSessionError::Protocol(
                        "peer sent an out-of-sequence table stream frame".into(),
                    ));
                },
            }
        }
    }
    if read_before(recv, idle_timeout).await? != TableFrame::Done {
        return Err(TableSessionError::Protocol("peer did not finish after its streams".into()));
    }
    Ok((received, newly_stored, continuation_pending))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainPlan {
    Complete,
    Send(ChainStart),
    Pending,
    /// The peer's copy of the chain diverged from ours at its tip: nothing we hold continues it.
    Diverged,
}

fn chain_plan(local: &ChainHead, frontier: FrontierState) -> Result<ChainPlan, TableSessionError> {
    match frontier {
        FrontierState::Empty => Ok(ChainPlan::Send(ChainStart::Beginning)),
        FrontierState::Accepted { lamport, .. } if lamport > local.lamport =>
            Ok(ChainPlan::Complete),
        FrontierState::Accepted { lamport, entry_hash } if lamport == local.lamport =>
            Ok(if entry_hash == local.entry_hash {
                ChainPlan::Complete
            } else {
                ChainPlan::Diverged
            }),
        FrontierState::Accepted { lamport, entry_hash } => {
            if let Some((floor_lamport, floor_hash)) = local.floor
                && lamport < floor_lamport
            {
                // The peer's accepted tip fell below our retained floor: offering the suffix
                // after its tip parks forever — every entry's predecessor was compacted away.
                // Offer the floor as a re-root instead; the receiver adopts it as its new chain
                // root and converges from there (#1127).
                return Ok(ChainPlan::Send(ChainStart::At {
                    lamport: floor_lamport,
                    entry_hash: floor_hash,
                }));
            }
            Ok(ChainPlan::Send(ChainStart::After { lamport, entry_hash }))
        },
        FrontierState::Restore { lamport, .. } if lamport > local.lamport => Ok(ChainPlan::Pending),
        FrontierState::Restore { lamport, entry_hash } => {
            // A witness below our retained floor has neither its entry nor its direct successor
            // here any more: re-root the restored receiver onto the floor, as the accepted arm
            // does (#1481). The floor is above the witness, so the receiver adopts it.
            if let Some((floor_lamport, floor_hash)) = local.floor
                && lamport < floor_lamport
            {
                return Ok(ChainPlan::Send(ChainStart::At {
                    lamport: floor_lamport,
                    entry_hash: floor_hash,
                }));
            }
            Ok(ChainPlan::Send(ChainStart::At { lamport, entry_hash }))
        },
    }
}

fn validate_chain_page(chains: &[ChainHead]) -> anyhow::Result<()> {
    anyhow::ensure!(
        chains.windows(2).all(|pair| pair[0].device_fingerprint < pair[1].device_fingerprint),
        "local table-sync chain page is not canonical"
    );
    Ok(())
}

async fn send_ack<W: AsyncWrite + Unpin>(
    send: &mut W,
    idle_timeout: Duration,
) -> Result<(), TableSessionError> {
    write_before(send, &TableFrame::Ack, idle_timeout).await?;
    auth::within(idle_timeout, send.shutdown(), || TableSessionError::Timeout {
        after: idle_timeout,
    })
    .await?
    .map_err(|error| TableSessionError::Codec(TableCodecError::Io(error)))
}

async fn write_before<W: AsyncWrite + Unpin>(
    send: &mut W,
    frame: &TableFrame,
    idle_timeout: Duration,
) -> Result<(), TableSessionError> {
    auth::within(idle_timeout, table_codec::write_frame(send, frame), || {
        TableSessionError::Timeout { after: idle_timeout }
    })
    .await?
    .map_err(TableSessionError::Codec)
}

async fn read_ack<R: AsyncRead + Unpin>(
    recv: &mut R,
    idle_timeout: Duration,
) -> Result<(), TableSessionError> {
    if read_before(recv, idle_timeout).await? == TableFrame::Ack {
        Ok(())
    } else {
        Err(TableSessionError::Protocol(
            "peer did not send the table-session acknowledgement".into(),
        ))
    }
}

async fn read_before<R: AsyncRead + Unpin>(
    recv: &mut R,
    idle_timeout: Duration,
) -> Result<TableFrame, TableSessionError> {
    let read = auth::within(idle_timeout, table_codec::read_frame(recv), || {
        TableSessionError::Timeout { after: idle_timeout }
    });
    match read.await? {
        Ok(frame) => Ok(frame),
        Err(TableCodecError::Eof) =>
            Err(TableSessionError::Protocol("peer closed before table-session completion".into())),
        Err(error) => Err(TableSessionError::Codec(error)),
    }
}

#[cfg(test)]
mod tests;
