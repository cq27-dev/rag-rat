//! Authenticated multi-stream table reconciliation over one bidirectional stream.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::auth::{AuthRole, SessionCapabilities};
use crate::session::{DEFAULT_IDLE_TIMEOUT, Ingested, MAX_SESSION_ENTRIES};
use crate::table_codec::{self, TableCodecError};
use crate::table_wire::{
    MAX_TABLE_ENTRIES_PER_PAGE, MAX_TABLE_ENTRY_BYTES, MAX_TABLE_INVENTORY_HASHES, Manifest,
    ManifestItem, TableFrame,
};

type Hash = [u8; 32];

/// Store operations needed by the stream-qualified table session.
pub trait TableSyncStore {
    fn account_id(&self) -> Hash;
    fn supported_streams(&self) -> anyhow::Result<Vec<ManifestItem>>;
    fn validates(&self, item: &ManifestItem) -> anyhow::Result<bool>;
    fn snapshot(&self, item: &ManifestItem) -> anyhow::Result<Vec<(Hash, Vec<u8>)>>;
    fn ingest(&mut self, item: &ManifestItem, signed_bytes: &[u8]) -> anyhow::Result<Ingested>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TableSessionReport {
    pub streams: usize,
    pub entries_sent: usize,
    pub entries_received: usize,
    pub entries_newly_stored: usize,
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
    mut send: W,
    mut recv: R,
    role: AuthRole,
    capabilities: SessionCapabilities,
    idle_timeout: Duration,
) -> Result<TableSessionReport, TableSessionError>
where
    S: TableSyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let local_manifest =
        Manifest::new(store.supported_streams().map_err(TableSessionError::Store)?)
            .map_err(|error| TableSessionError::Store(error.into()))?;
    let local_routes: HashSet<ManifestItem> = local_manifest.items().iter().cloned().collect();
    let mut snapshots = HashMap::with_capacity(local_manifest.items().len());
    for item in local_manifest.items() {
        let snapshot = store.snapshot(item).map_err(TableSessionError::Store)?;
        if snapshot.iter().any(|(_, bytes)| bytes.len() > MAX_TABLE_ENTRY_BYTES) {
            return Err(TableSessionError::Store(anyhow::anyhow!(
                "local table-sync entry exceeds {MAX_TABLE_ENTRY_BYTES} bytes"
            )));
        }
        snapshots.insert(item.stream_id, snapshot);
    }

    let (intersection_tx, intersection_rx) = tokio::sync::oneshot::channel::<Vec<ManifestItem>>();
    let (inventory_tx, mut inventory_rx) =
        tokio::sync::mpsc::channel::<(Hash, HashSet<Hash>)>(local_manifest.items().len().max(1));

    let sender = async move {
        write_before(&mut send, &TableFrame::Manifest(local_manifest), idle_timeout).await?;
        let Ok(intersection) = intersection_rx.await else {
            return Ok((send, 0usize));
        };
        for item in &intersection {
            let snapshot = snapshots.get(&item.stream_id).ok_or_else(|| {
                TableSessionError::Protocol("intersection names an unsupported local stream".into())
            })?;
            let have =
                snapshot.iter().map(|(hash, _)| *hash).take(MAX_TABLE_INVENTORY_HASHES).collect();
            write_before(
                &mut send,
                &TableFrame::Inventory { stream_id: item.stream_id, have },
                idle_timeout,
            )
            .await?;
        }

        let mut sent = 0;
        for item in &intersection {
            let Some((stream_id, peer_have)) = inventory_rx.recv().await else {
                return Ok((send, sent));
            };
            if stream_id != item.stream_id {
                return Err(TableSessionError::Protocol(
                    "peer inventories arrived out of canonical stream order".into(),
                ));
            }
            let remaining = MAX_SESSION_ENTRIES.saturating_sub(sent);
            let mut entries: Vec<Vec<u8>> = if capabilities.local.can_push() {
                snapshots[&item.stream_id]
                    .iter()
                    .filter(|(hash, _)| !peer_have.contains(hash))
                    .take(remaining)
                    .map(|(_, bytes)| bytes.clone())
                    .collect()
            } else {
                Vec::new()
            };
            sent += entries.len();
            while !entries.is_empty() {
                let tail = entries.split_off(entries.len().min(MAX_TABLE_ENTRIES_PER_PAGE));
                let page = std::mem::replace(&mut entries, tail);
                let more = !entries.is_empty();
                write_before(
                    &mut send,
                    &TableFrame::Entries { stream_id: item.stream_id, entries: page, more },
                    idle_timeout,
                )
                .await?;
            }
            write_before(
                &mut send,
                &TableFrame::StreamDone { stream_id: item.stream_id },
                idle_timeout,
            )
            .await?;
        }
        write_before(&mut send, &TableFrame::Done, idle_timeout).await?;
        Ok::<_, TableSessionError>((send, sent))
    };

    let receiver = async {
        let TableFrame::Manifest(peer_manifest) = read_before(&mut recv, idle_timeout).await?
        else {
            return Err(TableSessionError::Protocol("peer did not open with a manifest".into()));
        };
        let mut intersection = Vec::new();
        for item in peer_manifest.items() {
            if local_routes.contains(item)
                && store.validates(item).map_err(TableSessionError::Store)?
            {
                intersection.push(item.clone());
            }
        }
        let streams = intersection.len();
        intersection_tx.send(intersection.clone()).map_err(|_| {
            TableSessionError::Protocol("local sender stopped before manifest exchange".into())
        })?;

        for item in &intersection {
            let TableFrame::Inventory { stream_id, have } =
                read_before(&mut recv, idle_timeout).await?
            else {
                return Err(TableSessionError::Protocol(
                    "peer did not send the expected stream inventory".into(),
                ));
            };
            if stream_id != item.stream_id {
                return Err(TableSessionError::Protocol(
                    "peer inventory names the wrong stream".into(),
                ));
            }
            inventory_tx.send((stream_id, have.into_iter().collect())).await.map_err(|_| {
                TableSessionError::Protocol("local sender stopped during inventory exchange".into())
            })?;
        }

        let mut received = 0;
        let mut newly_stored = 0;
        for item in &intersection {
            let mut saw_page = false;
            let mut saw_final = false;
            loop {
                match read_before(&mut recv, idle_timeout).await? {
                    TableFrame::Entries { stream_id, entries, more } => {
                        if !capabilities.peer.can_push() {
                            return Err(TableSessionError::UnauthorizedPush);
                        }
                        if stream_id != item.stream_id {
                            return Err(TableSessionError::Protocol(
                                "peer entry page names the wrong stream".into(),
                            ));
                        }
                        if entries.is_empty() || saw_final {
                            return Err(TableSessionError::Protocol(
                                "peer sent an empty or after-final table entry page".into(),
                            ));
                        }
                        for bytes in entries {
                            received += 1;
                            if received > MAX_SESSION_ENTRIES {
                                return Err(TableSessionError::Protocol(format!(
                                    "peer streamed more than {MAX_SESSION_ENTRIES} table entries"
                                )));
                            }
                            if store.ingest(item, &bytes).map_err(TableSessionError::Store)?
                                == Ingested::Stored
                            {
                                newly_stored += 1;
                            }
                        }
                        saw_page = true;
                        saw_final = !more;
                    },
                    TableFrame::StreamDone { stream_id } => {
                        if stream_id != item.stream_id || (saw_page && !saw_final) {
                            return Err(TableSessionError::Protocol(
                                "peer ended the wrong or incomplete table stream".into(),
                            ));
                        }
                        break;
                    },
                    _ => {
                        return Err(TableSessionError::Protocol(
                            "peer sent an out-of-sequence table frame".into(),
                        ));
                    },
                }
            }
        }
        if read_before(&mut recv, idle_timeout).await? != TableFrame::Done {
            return Err(TableSessionError::Protocol(
                "peer did not finish after its streams".into(),
            ));
        }
        Ok::<_, TableSessionError>((recv, streams, received, newly_stored))
    };

    let ((mut send, entries_sent), (mut recv, streams, entries_received, entries_newly_stored)) =
        tokio::try_join!(sender, receiver)?;
    complete(&mut send, &mut recv, role, idle_timeout).await?;
    Ok(TableSessionReport { streams, entries_sent, entries_received, entries_newly_stored })
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
    use super::*;

    #[derive(Clone)]
    struct MemStore {
        account: Hash,
        supported: Vec<ManifestItem>,
        entries: HashMap<Hash, HashMap<Hash, Vec<u8>>>,
    }

    impl MemStore {
        fn new(items: Vec<ManifestItem>) -> Self {
            Self { account: [7; 32], supported: items, entries: HashMap::new() }
        }

        fn insert(&mut self, stream: Hash, seed: u8) {
            let mut bytes = vec![seed; 40];
            bytes[..32].copy_from_slice(&[seed; 32]);
            self.entries.entry(stream).or_default().insert([seed; 32], bytes);
        }
    }

    impl TableSyncStore for MemStore {
        fn account_id(&self) -> Hash {
            self.account
        }

        fn supported_streams(&self) -> anyhow::Result<Vec<ManifestItem>> {
            Ok(self.supported.clone())
        }

        fn validates(&self, item: &ManifestItem) -> anyhow::Result<bool> {
            Ok(self.supported.contains(item))
        }

        fn snapshot(&self, item: &ManifestItem) -> anyhow::Result<Vec<(Hash, Vec<u8>)>> {
            Ok(self
                .entries
                .get(&item.stream_id)
                .into_iter()
                .flatten()
                .map(|(hash, bytes)| (*hash, bytes.clone()))
                .collect())
        }

        fn ingest(&mut self, item: &ManifestItem, bytes: &[u8]) -> anyhow::Result<Ingested> {
            if !self.supported.contains(item) {
                return Ok(Ingested::NoChange);
            }
            let hash: Hash = bytes[..32].try_into()?;
            Ok(match self.entries.entry(item.stream_id).or_default().entry(hash) {
                std::collections::hash_map::Entry::Occupied(_) => Ingested::NoChange,
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(bytes.to_vec());
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
        let (a_send, b_recv) = tokio::io::duplex(1 << 20);
        let (b_send, a_recv) = tokio::io::duplex(1 << 20);
        let (a, b) = tokio::join!(
            run_table_session(
                a,
                a_send,
                a_recv,
                AuthRole::Dialer,
                SessionCapabilities::bidirectional(),
            ),
            run_table_session(
                b,
                b_send,
                b_recv,
                AuthRole::Acceptor,
                SessionCapabilities::bidirectional(),
            ),
        );
        (a.unwrap(), b.unwrap())
    }

    #[tokio::test]
    async fn only_the_multi_repo_manifest_intersection_reconciles() {
        let shared = item("repo-b", 2);
        let mut a = MemStore::new(vec![item("repo-a", 1), shared.clone()]);
        let mut b = MemStore::new(vec![shared.clone(), item("repo-c", 3)]);
        a.insert([1; 32], 10);
        a.insert(shared.stream_id, 20);
        b.insert([3; 32], 30);

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
                TableFrame::Inventory { stream_id: shared.stream_id, have: Vec::new() },
                TableFrame::StreamDone { stream_id: shared.stream_id },
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
