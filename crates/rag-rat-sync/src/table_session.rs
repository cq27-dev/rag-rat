//! Authenticated multi-stream table reconciliation over one bidirectional stream.

use std::collections::HashSet;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::auth::{AuthRole, SessionCapabilities};
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
        expected_device: Hash,
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
}

#[derive(Debug)]
pub enum TableSessionError {
    Codec(TableCodecError),
    Protocol(String),
    UnauthorizedPush,
    Store(anyhow::Error),
}

impl std::fmt::Display for TableSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codec(error) => write!(f, "table-sync session transport: {error}"),
            Self::Protocol(message) => write!(f, "table-sync protocol violation: {message}"),
            Self::UnauthorizedPush => write!(f, "read-only peer attempted to push table entries"),
            Self::Store(error) => write!(f, "table-sync session store: {error}"),
        }
    }
}

impl std::error::Error for TableSessionError {}

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
    run_table_session_with_idle_timeout(store, send, recv, role, capabilities, DEFAULT_IDLE_TIMEOUT)
        .await
}

async fn run_table_session_with_idle_timeout<S, R, W>(
    store: &mut S,
    send: W,
    recv: R,
    role: AuthRole,
    capabilities: SessionCapabilities,
    idle_timeout: Duration,
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
        idle_timeout,
        TableSessionLimits::default(),
    )
    .await
}

#[derive(Clone, Copy)]
struct TableSessionLimits {
    chains_per_page: usize,
    chains_per_session: usize,
    entries_per_page: usize,
    entries_per_session: usize,
}

impl Default for TableSessionLimits {
    fn default() -> Self {
        Self {
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
    idle_timeout: Duration,
    limits: TableSessionLimits,
) -> Result<TableSessionReport, TableSessionError>
where
    S: TableSyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
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
    let (entries_sent, entries_received, entries_newly_stored, continuation_pending) = match role {
        AuthRole::Dialer => {
            let (entries_sent, local_pending) = send_direction(
                store,
                &intersection,
                &mut send,
                &mut recv,
                capabilities.local.can_push(),
                idle_timeout,
                limits,
            )
            .await?;
            let (entries_received, entries_newly_stored, peer_pending) = receive_direction(
                store,
                &intersection,
                &mut send,
                &mut recv,
                capabilities.peer.can_push(),
                idle_timeout,
                limits,
            )
            .await?;
            (entries_sent, entries_received, entries_newly_stored, local_pending || peer_pending)
        },
        AuthRole::Acceptor => {
            let (entries_received, entries_newly_stored, peer_pending) = receive_direction(
                store,
                &intersection,
                &mut send,
                &mut recv,
                capabilities.peer.can_push(),
                idle_timeout,
                limits,
            )
            .await?;
            let (entries_sent, local_pending) = send_direction(
                store,
                &intersection,
                &mut send,
                &mut recv,
                capabilities.local.can_push(),
                idle_timeout,
                limits,
            )
            .await?;
            (entries_sent, entries_received, entries_newly_stored, local_pending || peer_pending)
        },
    };
    complete(&mut send, &mut recv, role, idle_timeout).await?;
    Ok(TableSessionReport {
        streams,
        entries_sent,
        entries_received,
        entries_newly_stored,
        continuation_pending,
    })
}

async fn send_direction<S, R, W>(
    store: &S,
    streams: &[ManifestItem],
    send: &mut W,
    recv: &mut R,
    can_push: bool,
    idle_timeout: Duration,
    limits: TableSessionLimits,
) -> Result<(usize, bool), TableSessionError>
where
    S: TableSyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut sent = 0;
    let mut offered_chains: usize = 0;
    let mut continuation_pending = false;
    for item in streams {
        let mut after_device = None;
        let mut stream_pending = false;
        loop {
            if !can_push {
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
                let mut start = match chain_plan(chain, frontier.state)? {
                    ChainPlan::Complete => continue,
                    ChainPlan::Send(start) => start,
                    ChainPlan::Pending => {
                        stream_pending = true;
                        continue;
                    },
                };
                while sent < limits.entries_per_session {
                    let page_limit = limits
                        .entries_per_page
                        .min(limits.entries_per_session.saturating_sub(sent));
                    let entries = store
                        .entries(item, chain.device_fingerprint, start, page_limit)
                        .map_err(TableSessionError::Store)?;
                    if entries.is_empty() {
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
    Ok((sent, continuation_pending))
}

async fn receive_direction<S, R, W>(
    store: &mut S,
    streams: &[ManifestItem],
    send: &mut W,
    recv: &mut R,
    peer_can_push: bool,
    idle_timeout: Duration,
    limits: TableSessionLimits,
) -> Result<(usize, usize, bool), TableSessionError>
where
    S: TableSyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut received = 0;
    let mut newly_stored = 0;
    let mut offered_chains: usize = 0;
    let mut continuation_pending = false;
    for item in streams {
        let mut last_device = None;
        loop {
            match read_before(recv, idle_timeout).await? {
                TableFrame::ChainInventory { stream_id, chains } => {
                    if !peer_can_push {
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
                                for bytes in entries {
                                    if store
                                        .ingest(item, device_fingerprint, &bytes)
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
}

fn chain_plan(local: &ChainHead, frontier: FrontierState) -> Result<ChainPlan, TableSessionError> {
    match frontier {
        FrontierState::Empty => Ok(ChainPlan::Send(ChainStart::Beginning)),
        FrontierState::Accepted { lamport, .. } if lamport > local.lamport =>
            Ok(ChainPlan::Complete),
        FrontierState::Accepted { lamport, entry_hash } if lamport == local.lamport => {
            if entry_hash != local.entry_hash {
                return Err(TableSessionError::Protocol(
                    "peer chain frontier conflicts with the offered tip".into(),
                ));
            }
            Ok(ChainPlan::Complete)
        },
        FrontierState::Accepted { lamport, entry_hash } =>
            Ok(ChainPlan::Send(ChainStart::After { lamport, entry_hash })),
        FrontierState::Restore { lamport, .. } if lamport > local.lamport => Ok(ChainPlan::Pending),
        FrontierState::Restore { lamport, entry_hash } =>
            Ok(ChainPlan::Send(ChainStart::At { lamport, entry_hash })),
    }
}

fn validate_chain_page(chains: &[ChainHead]) -> anyhow::Result<()> {
    anyhow::ensure!(
        chains.windows(2).all(|pair| pair[0].device_fingerprint < pair[1].device_fingerprint),
        "local table-sync chain page is not canonical"
    );
    Ok(())
}

async fn complete<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    send: &mut W,
    recv: &mut R,
    role: AuthRole,
    idle_timeout: Duration,
) -> Result<(), TableSessionError> {
    match role {
        AuthRole::Dialer => {
            send_ack(send, idle_timeout).await?;
            read_ack(recv, idle_timeout).await
        },
        AuthRole::Acceptor => {
            read_ack(recv, idle_timeout).await?;
            send_ack(send, idle_timeout).await
        },
    }
}

async fn send_ack<W: AsyncWrite + Unpin>(
    send: &mut W,
    idle_timeout: Duration,
) -> Result<(), TableSessionError> {
    write_before(send, &TableFrame::Ack, idle_timeout).await?;
    match tokio::time::timeout(idle_timeout, send.shutdown()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(TableSessionError::Codec(TableCodecError::Io(error))),
        Err(_) => Err(TableSessionError::Protocol("table session timed out while closing".into())),
    }
}

async fn write_before<W: AsyncWrite + Unpin>(
    send: &mut W,
    frame: &TableFrame,
    idle_timeout: Duration,
) -> Result<(), TableSessionError> {
    match tokio::time::timeout(idle_timeout, table_codec::write_frame(send, frame)).await {
        Ok(result) => result.map_err(TableSessionError::Codec),
        Err(_) => Err(TableSessionError::Protocol("table session timed out while writing".into())),
    }
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
    match tokio::time::timeout(idle_timeout, table_codec::read_frame(recv)).await {
        Ok(Ok(frame)) => Ok(frame),
        Ok(Err(TableCodecError::Eof)) =>
            Err(TableSessionError::Protocol("peer closed before table-session completion".into())),
        Ok(Err(error)) => Err(TableSessionError::Codec(error)),
        Err(_) => Err(TableSessionError::Protocol("table session timed out as idle".into())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;

    #[derive(Clone)]
    struct TestEntry {
        device: Hash,
        lamport: u64,
        bytes: Vec<u8>,
    }

    #[derive(Clone)]
    struct MemStore {
        account: Hash,
        supported: Vec<ManifestItem>,
        entries: HashMap<Hash, HashMap<Hash, TestEntry>>,
        forbidden_snapshots: HashSet<Hash>,
        prepare_count: usize,
    }

    impl MemStore {
        fn new(items: Vec<ManifestItem>) -> Self {
            Self {
                account: [7; 32],
                supported: items,
                entries: HashMap::new(),
                forbidden_snapshots: HashSet::new(),
                prepare_count: 0,
            }
        }

        fn insert(&mut self, stream: Hash, seed: u8) {
            self.insert_chain(stream, seed, 0, seed);
        }

        fn insert_chain(&mut self, stream: Hash, device: u8, lamport: u64, seed: u8) {
            let mut bytes = vec![seed; 41];
            bytes[..32].copy_from_slice(&[seed; 32]);
            bytes[32] = device;
            bytes[33..41].copy_from_slice(&lamport.to_be_bytes());
            self.entries.entry(stream).or_default().insert([seed; 32], TestEntry {
                device: [device; 32],
                lamport,
                bytes,
            });
        }

        fn forbid_snapshot(&mut self, stream: Hash) {
            self.forbidden_snapshots.insert(stream);
        }
    }

    impl TableSyncStore for MemStore {
        fn account_id(&self) -> Hash {
            self.account
        }

        fn prepare(&mut self) -> anyhow::Result<()> {
            self.prepare_count += 1;
            Ok(())
        }

        fn supported_streams(&self) -> anyhow::Result<Vec<ManifestItem>> {
            Ok(self.supported.clone())
        }

        fn validates(&self, item: &ManifestItem) -> anyhow::Result<bool> {
            Ok(self.supported.contains(item))
        }

        fn chain_page(
            &self,
            item: &ManifestItem,
            after_device: Option<Hash>,
            limit: usize,
        ) -> anyhow::Result<Vec<ChainHead>> {
            anyhow::ensure!(
                !self.forbidden_snapshots.contains(&item.stream_id),
                "non-intersecting stream was snapshotted"
            );
            let mut chains = BTreeMap::new();
            for (hash, entry) in self.entries.get(&item.stream_id).into_iter().flatten() {
                let head = chains.entry(entry.device).or_insert((entry.lamport, *hash));
                if entry.lamport > head.0 {
                    *head = (entry.lamport, *hash);
                }
            }
            Ok(chains
                .into_iter()
                .filter(|(device, _)| after_device.is_none_or(|after| *device > after))
                .take(limit)
                .map(|(device, (lamport, entry_hash))| ChainHead {
                    device_fingerprint: device,
                    lamport,
                    entry_hash,
                })
                .collect())
        }

        fn frontier(&self, item: &ManifestItem, device: Hash) -> anyhow::Result<FrontierState> {
            Ok(self
                .entries
                .get(&item.stream_id)
                .into_iter()
                .flatten()
                .filter(|(_, entry)| entry.device == device)
                .max_by_key(|(_, entry)| entry.lamport)
                .map_or(FrontierState::Empty, |(hash, entry)| FrontierState::Accepted {
                    lamport: entry.lamport,
                    entry_hash: *hash,
                }))
        }

        fn entries(
            &self,
            item: &ManifestItem,
            device: Hash,
            start: ChainStart,
            limit: usize,
        ) -> anyhow::Result<Vec<ChainEntry>> {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let entries = self.entries.get(&item.stream_id);
            let (minimum, inclusive) = match start {
                ChainStart::Beginning => (None, false),
                ChainStart::After { lamport, entry_hash } => {
                    if !entries.is_some_and(|entries| {
                        entries
                            .get(&entry_hash)
                            .is_some_and(|entry| entry.device == device && entry.lamport == lamport)
                    }) {
                        anyhow::bail!("test cursor is not present")
                    }
                    (Some(lamport), false)
                },
                ChainStart::At { lamport, entry_hash } => {
                    if !entries.is_some_and(|entries| {
                        entries
                            .get(&entry_hash)
                            .is_some_and(|entry| entry.device == device && entry.lamport == lamport)
                    }) {
                        anyhow::bail!("test restore cursor is not present")
                    }
                    (Some(lamport), true)
                },
            };
            let mut chain: Vec<_> =
                entries.into_iter().flatten().filter(|(_, entry)| entry.device == device).collect();
            chain.sort_by_key(|(_, entry)| entry.lamport);
            Ok(chain
                .into_iter()
                .filter(|(_, entry)| {
                    minimum.is_none_or(|minimum| {
                        entry.lamport > minimum || (inclusive && entry.lamport == minimum)
                    })
                })
                .take(limit)
                .map(|(hash, entry)| ChainEntry {
                    lamport: entry.lamport,
                    entry_hash: *hash,
                    signed_bytes: entry.bytes.clone(),
                })
                .collect())
        }

        fn ingest(
            &mut self,
            item: &ManifestItem,
            expected_device: Hash,
            bytes: &[u8],
        ) -> anyhow::Result<Ingested> {
            if !self.supported.contains(item) {
                return Ok(Ingested::NoChange);
            }
            let hash: Hash = bytes[..32].try_into()?;
            let device = [bytes[32]; 32];
            let lamport = u64::from_be_bytes(bytes[33..41].try_into()?);
            if device != expected_device {
                return Ok(Ingested::NoChange);
            }
            Ok(match self.entries.entry(item.stream_id).or_default().entry(hash) {
                std::collections::hash_map::Entry::Occupied(_) => Ingested::NoChange,
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(TestEntry { device, lamport, bytes: bytes.to_vec() });
                    Ingested::Stored
                },
            })
        }
    }

    fn item(repo: &str, stream: u8) -> ManifestItem {
        ManifestItem {
            repo_id: repo.into(),
            incarnation_ref: [1; 32],
            scope_id: "anchors/1".into(),
            stream_id: [stream; 32],
        }
    }

    async fn pair(a: &mut MemStore, b: &mut MemStore) -> (TableSessionReport, TableSessionReport) {
        pair_with_limits(a, b, TableSessionLimits::default()).await
    }

    async fn pair_with_limits(
        a: &mut MemStore,
        b: &mut MemStore,
        limits: TableSessionLimits,
    ) -> (TableSessionReport, TableSessionReport) {
        let (a, b) = try_pair_with_limits(a, b, limits).await;
        (a.unwrap(), b.unwrap())
    }

    async fn try_pair_with_limits(
        a: &mut MemStore,
        b: &mut MemStore,
        limits: TableSessionLimits,
    ) -> (
        Result<TableSessionReport, TableSessionError>,
        Result<TableSessionReport, TableSessionError>,
    ) {
        let (a_send, b_recv) = tokio::io::duplex(1 << 20);
        let (b_send, a_recv) = tokio::io::duplex(1 << 20);
        tokio::join!(
            run_table_session_with_limits(
                a,
                a_send,
                a_recv,
                AuthRole::Dialer,
                SessionCapabilities::bidirectional(),
                DEFAULT_IDLE_TIMEOUT,
                limits,
            ),
            run_table_session_with_limits(
                b,
                b_send,
                b_recv,
                AuthRole::Acceptor,
                SessionCapabilities::bidirectional(),
                DEFAULT_IDLE_TIMEOUT,
                limits,
            ),
        )
    }

    #[tokio::test]
    async fn only_the_multi_repo_manifest_intersection_reconciles() {
        let shared = item("repo-b", 2);
        let mut a = MemStore::new(vec![item("repo-a", 1), shared.clone()]);
        let mut b = MemStore::new(vec![shared.clone(), item("repo-c", 3)]);
        a.insert([1; 32], 10);
        a.insert(shared.stream_id, 20);
        b.insert([3; 32], 30);
        a.forbid_snapshot([1; 32]);
        b.forbid_snapshot([3; 32]);

        let (a_report, b_report) = pair(&mut a, &mut b).await;
        assert_eq!(a_report.streams, 1);
        assert_eq!(b_report.entries_newly_stored, 1);
        assert_eq!(b.entries[&shared.stream_id].len(), 1);
        assert!(!b.entries.contains_key(&[1; 32]), "repo-a never crosses into repo-c's peer");
        assert!(!a.entries.contains_key(&[3; 32]), "repo-c never crosses into repo-a's peer");

        let (again_a, again_b) = pair(&mut a, &mut b).await;
        assert_eq!(again_a.entries_sent + again_b.entries_sent, 0);
        assert_eq!(again_a.entries_newly_stored + again_b.entries_newly_stored, 0);
    }

    #[tokio::test]
    async fn empty_manifests_are_a_clean_no_op() {
        let mut a = MemStore::new(Vec::new());
        let mut b = MemStore::new(Vec::new());
        let (a, b) = pair(&mut a, &mut b).await;
        assert_eq!(a, TableSessionReport::default());
        assert_eq!(b, TableSessionReport::default());
    }

    #[tokio::test]
    async fn read_only_sessions_do_not_prepare_local_table_authorship() {
        use crate::auth::PeerCapability;

        let mut a = MemStore::new(Vec::new());
        let mut b = MemStore::new(Vec::new());
        let (a_send, b_recv) = tokio::io::duplex(1024);
        let (b_send, a_recv) = tokio::io::duplex(1024);
        let capabilities =
            SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadOnly);
        let (a_result, b_result) = tokio::join!(
            run_table_session_with_limits(
                &mut a,
                a_send,
                a_recv,
                AuthRole::Dialer,
                capabilities,
                DEFAULT_IDLE_TIMEOUT,
                TableSessionLimits::default(),
            ),
            run_table_session_with_limits(
                &mut b,
                b_send,
                b_recv,
                AuthRole::Acceptor,
                capabilities,
                DEFAULT_IDLE_TIMEOUT,
                TableSessionLimits::default(),
            ),
        );
        a_result.unwrap();
        b_result.unwrap();
        assert_eq!(a.prepare_count, 0);
        assert_eq!(b.prepare_count, 0);
    }

    #[tokio::test]
    async fn capped_sessions_advance_from_durable_frontiers_until_quiet() {
        let shared = item("repo-a", 1);
        let mut source = MemStore::new(vec![shared.clone()]);
        let mut destination = MemStore::new(vec![shared.clone()]);
        for seed in 1..=5 {
            source.insert_chain(shared.stream_id, 7, u64::from(seed), seed);
        }
        let limits = TableSessionLimits {
            chains_per_page: 2,
            chains_per_session: 8,
            entries_per_page: 1,
            entries_per_session: 2,
        };

        let mut moved = Vec::new();
        let mut pending = Vec::new();
        for _ in 0..4 {
            let (source_report, destination_report) =
                pair_with_limits(&mut source, &mut destination, limits).await;
            moved.push(source_report.entries_sent);
            pending.push(source_report.continuation_pending);
            assert_eq!(source_report.entries_sent, destination_report.entries_newly_stored);
        }
        assert_eq!(moved, [2, 2, 1, 0]);
        assert_eq!(pending, [true, true, false, false]);
        assert_eq!(destination.entries[&shared.stream_id].len(), 5);
    }

    #[tokio::test]
    async fn lost_completion_ack_does_not_consume_or_repeat_progress() {
        let shared = item("repo-a", 1);
        let mut source = MemStore::new(vec![shared.clone()]);
        let mut destination = MemStore::new(vec![shared.clone()]);
        source.insert(shared.stream_id, 1);
        let limits = TableSessionLimits::default();
        let (mut source_send, mut destination_recv) = tokio::io::duplex(4096);
        let (mut destination_send, mut source_recv) = tokio::io::duplex(4096);
        let streams = vec![shared];
        let (sent, received) = tokio::join!(
            send_direction(
                &source,
                &streams,
                &mut source_send,
                &mut source_recv,
                true,
                DEFAULT_IDLE_TIMEOUT,
                limits,
            ),
            receive_direction(
                &mut destination,
                &streams,
                &mut destination_send,
                &mut destination_recv,
                true,
                DEFAULT_IDLE_TIMEOUT,
                limits,
            ),
        );
        assert_eq!(sent.unwrap(), (1, false));
        assert_eq!(received.unwrap(), (1, 1, false));

        let (source_report, destination_report) = pair(&mut source, &mut destination).await;
        assert_eq!(source_report.entries_sent + destination_report.entries_sent, 0);
        assert_eq!(source_report.entries_newly_stored + destination_report.entries_newly_stored, 0);
    }

    #[test]
    fn peer_frontiers_must_be_provable_prefixes_and_restore_debt_stays_pending() {
        let local = ChainHead { device_fingerprint: [1; 32], lamport: 3, entry_hash: [3; 32] };
        assert!(matches!(
            chain_plan(&local, FrontierState::Accepted { lamport: 3, entry_hash: [4; 32] }),
            Err(TableSessionError::Protocol(_))
        ));
        assert_eq!(
            chain_plan(&local, FrontierState::Accepted { lamport: 4, entry_hash: [4; 32] })
                .unwrap(),
            ChainPlan::Complete
        );
        assert_eq!(
            chain_plan(&local, FrontierState::Accepted { lamport: 2, entry_hash: [2; 32] })
                .unwrap(),
            ChainPlan::Send(ChainStart::After { lamport: 2, entry_hash: [2; 32] })
        );
        assert_eq!(
            chain_plan(&local, FrontierState::Restore { lamport: 4, entry_hash: [4; 32] }).unwrap(),
            ChainPlan::Pending
        );
    }

    #[tokio::test]
    async fn local_chain_inventory_enforces_the_exact_session_ceiling() {
        let shared = item("repo-a", 1);
        let mut source = MemStore::new(vec![shared.clone()]);
        let mut destination = MemStore::new(vec![shared.clone()]);
        source.insert_chain(shared.stream_id, 1, 0, 1);
        source.insert_chain(shared.stream_id, 2, 0, 2);
        let limits = TableSessionLimits {
            chains_per_page: 1,
            chains_per_session: 2,
            entries_per_page: 1,
            entries_per_session: 3,
        };

        let (source_report, destination_report) =
            pair_with_limits(&mut source, &mut destination, limits).await;
        assert_eq!(source_report.entries_sent, 2);
        assert_eq!(destination_report.entries_newly_stored, 2);

        source.insert_chain(shared.stream_id, 3, 0, 3);
        let (source_result, peer_result) =
            try_pair_with_limits(&mut source, &mut destination, limits).await;
        assert!(matches!(
            source_result,
            Err(TableSessionError::Store(error)) if error.to_string().contains("ceiling")
        ));
        assert!(peer_result.is_err());
    }

    #[tokio::test]
    async fn peer_chain_inventory_must_advance_order_and_respect_the_session_ceiling() {
        for devices in [vec![2, 2], vec![1, 2, 3]] {
            let shared = item("repo-a", 1);
            let streams = vec![shared.clone()];
            let mut store = MemStore::new(streams.clone());
            let limits = TableSessionLimits {
                chains_per_page: 1,
                chains_per_session: 2,
                entries_per_page: 1,
                entries_per_session: 2,
            };
            let (mut receiver_send, mut peer_recv) = tokio::io::duplex(4096);
            let (mut peer_send, mut receiver_recv) = tokio::io::duplex(4096);
            let peer = async move {
                for device in &devices[..devices.len() - 1] {
                    table_codec::write_frame(&mut peer_send, &TableFrame::ChainInventory {
                        stream_id: shared.stream_id,
                        chains: vec![ChainHead {
                            device_fingerprint: [*device; 32],
                            lamport: 0,
                            entry_hash: [*device; 32],
                        }],
                    })
                    .await
                    .unwrap();
                    assert!(matches!(
                        table_codec::read_frame(&mut peer_recv).await.unwrap(),
                        TableFrame::ChainFrontiers { .. }
                    ));
                    table_codec::write_frame(&mut peer_send, &TableFrame::InventoryDone {
                        stream_id: shared.stream_id,
                    })
                    .await
                    .unwrap();
                }
                let device = devices[devices.len() - 1];
                table_codec::write_frame(&mut peer_send, &TableFrame::ChainInventory {
                    stream_id: shared.stream_id,
                    chains: vec![ChainHead {
                        device_fingerprint: [device; 32],
                        lamport: 0,
                        entry_hash: [device; 32],
                    }],
                })
                .await
                .unwrap();
            };
            let receiver = receive_direction(
                &mut store,
                &streams,
                &mut receiver_send,
                &mut receiver_recv,
                true,
                DEFAULT_IDLE_TIMEOUT,
                limits,
            );
            let (result, ()) = tokio::join!(receiver, peer);
            assert!(
                matches!(result, Err(TableSessionError::Protocol(message)) if message.contains("cap"))
            );
        }
    }

    #[tokio::test]
    async fn entry_pages_must_name_a_chain_in_the_current_inventory() {
        let shared = item("repo-a", 1);
        let streams = vec![shared.clone()];
        let mut store = MemStore::new(streams.clone());
        let (mut receiver_send, mut peer_recv) = tokio::io::duplex(4096);
        let (mut peer_send, mut receiver_recv) = tokio::io::duplex(4096);
        let peer = async move {
            table_codec::write_frame(&mut peer_send, &TableFrame::ChainInventory {
                stream_id: shared.stream_id,
                chains: vec![ChainHead {
                    device_fingerprint: [1; 32],
                    lamport: 0,
                    entry_hash: [1; 32],
                }],
            })
            .await
            .unwrap();
            assert!(matches!(
                table_codec::read_frame(&mut peer_recv).await.unwrap(),
                TableFrame::ChainFrontiers { .. }
            ));
            table_codec::write_frame(&mut peer_send, &TableFrame::Entries {
                stream_id: shared.stream_id,
                device_fingerprint: [2; 32],
                entries: vec![vec![0; 41]],
            })
            .await
            .unwrap();
        };
        let receiver = receive_direction(
            &mut store,
            &streams,
            &mut receiver_send,
            &mut receiver_recv,
            true,
            DEFAULT_IDLE_TIMEOUT,
            TableSessionLimits::default(),
        );
        let (result, ()) = tokio::join!(receiver, peer);
        assert!(matches!(result, Err(TableSessionError::Protocol(_))));
        assert!(store.entries.is_empty());
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_cannot_block_writes_forever() {
        let shared = item("repo-a", 1);
        let mut store = MemStore::new(vec![shared.clone()]);
        for seed in 1..=32 {
            store.insert(shared.stream_id, seed);
        }
        let (send, _peer_recv) = tokio::io::duplex(64);
        let (mut peer_send, recv) = tokio::io::duplex(4096);
        let peer = async move {
            for frame in [
                TableFrame::Manifest(Manifest::new(vec![shared.clone()]).unwrap()),
                TableFrame::ChainFrontiers {
                    stream_id: shared.stream_id,
                    frontiers: vec![ChainFrontier {
                        device_fingerprint: [1; 32],
                        state: FrontierState::Empty,
                    }],
                },
                TableFrame::StreamDone { stream_id: shared.stream_id, continuation_pending: false },
                TableFrame::Done,
            ] {
                table_codec::write_frame(&mut peer_send, &frame).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let (result, ()) = tokio::join!(
            run_table_session_with_idle_timeout(
                &mut store,
                send,
                recv,
                AuthRole::Dialer,
                SessionCapabilities::bidirectional(),
                Duration::from_millis(20),
            ),
            peer,
        );
        assert!(matches!(
            result,
            Err(TableSessionError::Protocol(message)) if message.contains("timed out while writing")
        ));
    }
}
