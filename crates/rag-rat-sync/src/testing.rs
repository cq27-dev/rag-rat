//! In-memory stores shared by the crate's unit tests: one per store seam, so each lane's session
//! logic and the endpoint dispatch run without a database.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::auth::{LocalAuth, NodeAuth, PeerAuthorization, PeerCapability};
use crate::session::{Ingested, ServeScope, SyncStore};
use crate::table_session::{ChainEntry, ChainStart, TableSyncStore};
use crate::table_wire::{ChainHead, FrontierState, ManifestItem};

type Hash = [u8; 32];

/// An in-memory store: entries are `(hash, bytes)`, ingest inserts if absent. Enough to
/// exercise the protocol without a database — the DB-backed store has its own integration
/// test.
pub(crate) struct SessionMemStore {
    pub(crate) account: Hash,
    pub(crate) entries: HashMap<Hash, Vec<u8>>,
}

impl SessionMemStore {
    pub(crate) fn new(account: Hash, entries: &[(Hash, Vec<u8>)]) -> Self {
        Self { account, entries: entries.iter().cloned().collect() }
    }
}

impl SyncStore for SessionMemStore {
    fn account_id(&self) -> Hash {
        self.account
    }
    fn set_serve_scope(&mut self, _scope: ServeScope) {}
    fn snapshot(&self) -> anyhow::Result<Vec<(Hash, Vec<u8>)>> {
        let mut v: Vec<_> = self.entries.iter().map(|(h, b)| (*h, b.clone())).collect();
        v.sort_by_key(|(h, _)| *h);
        Ok(v)
    }
    fn ingest(&mut self, signed_bytes: &[u8]) -> anyhow::Result<Ingested> {
        // The test's "hash" is the first 32 bytes of the payload it authored below.
        let hash: Hash = signed_bytes[..32].try_into().unwrap();
        match self.entries.entry(hash) {
            std::collections::hash_map::Entry::Occupied(_) => Ok(Ingested::NoChange),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(signed_bytes.to_vec());
                Ok(Ingested::Stored)
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct TestEntry {
    pub(crate) device: Hash,
    pub(crate) lamport: u64,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct TableMemStore {
    pub(crate) account: Hash,
    pub(crate) supported: Vec<ManifestItem>,
    pub(crate) entries: HashMap<Hash, HashMap<Hash, TestEntry>>,
    pub(crate) forbidden_snapshots: HashSet<Hash>,
    pub(crate) prepare_count: usize,
    pub(crate) owed_tips: HashMap<(Hash, Hash), (u64, Hash)>,
    /// Serve entries with a plain store error (a failure that is not a diverged cursor).
    pub(crate) fail_entries: bool,
}

impl TableMemStore {
    pub(crate) fn new(items: Vec<ManifestItem>) -> Self {
        Self {
            account: [7; 32],
            supported: items,
            entries: HashMap::new(),
            forbidden_snapshots: HashSet::new(),
            prepare_count: 0,
            owed_tips: HashMap::new(),
            fail_entries: false,
        }
    }

    pub(crate) fn insert(&mut self, stream: Hash, seed: u8) {
        self.insert_chain(stream, seed, 0, seed);
    }

    pub(crate) fn insert_chain(&mut self, stream: Hash, device: u8, lamport: u64, seed: u8) {
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

    pub(crate) fn forbid_snapshot(&mut self, stream: Hash) {
        self.forbidden_snapshots.insert(stream);
    }
}

impl TableSyncStore for TableMemStore {
    fn has_pending_coverage(&self, item: &ManifestItem) -> anyhow::Result<bool> {
        Ok(self.owed_tips.keys().any(|(stream, _)| *stream == item.stream_id))
    }
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
        for (&(stream, device), &tip) in &self.owed_tips {
            if stream == item.stream_id {
                chains.insert(device, tip);
            }
        }
        Ok(chains
            .into_iter()
            .filter(|(device, _)| after_device.is_none_or(|after| *device > after))
            .take(limit)
            .map(|(device, (lamport, entry_hash))| ChainHead {
                floor: None,
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
        anyhow::ensure!(!self.fail_entries, "database is locked");
        let entries = self.entries.get(&item.stream_id);
        let (minimum, inclusive) = match start {
            ChainStart::Beginning => (None, false),
            ChainStart::After { lamport, entry_hash } => {
                if !entries.is_some_and(|entries| {
                    entries
                        .get(&entry_hash)
                        .is_some_and(|entry| entry.device == device && entry.lamport == lamport)
                }) {
                    if self
                        .owed_tips
                        .get(&(item.stream_id, device))
                        .is_some_and(|(tip, _)| lamport <= *tip)
                    {
                        return Ok(Vec::new());
                    }
                    return Err(rag_rat_oplog::UnservableChainCursor::NotHeld.into());
                }
                (Some(lamport), false)
            },
            ChainStart::At { lamport, entry_hash } => {
                if !entries.is_some_and(|entries| {
                    entries
                        .get(&entry_hash)
                        .is_some_and(|entry| entry.device == device && entry.lamport == lamport)
                }) {
                    return Err(rag_rat_oplog::UnservableChainCursor::NoRestoreSuccessor.into());
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
        offered: &crate::table_wire::ChainHead,
        bytes: &[u8],
    ) -> anyhow::Result<Ingested> {
        if !self.supported.contains(item) {
            return Ok(Ingested::NoChange);
        }
        let hash: Hash = bytes[..32].try_into()?;
        let device = [bytes[32]; 32];
        let lamport = u64::from_be_bytes(bytes[33..41].try_into()?);
        if device != offered.device_fingerprint {
            return Ok(Ingested::NoChange);
        }
        if self.owed_tips.get(&(item.stream_id, device)) == Some(&(lamport, hash)) {
            self.owed_tips.remove(&(item.stream_id, device));
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

pub(crate) struct TestStore {
    pub(crate) account: [u8; 32],
    pub(crate) entries: HashMap<[u8; 32], Vec<u8>>,
    pub(crate) local_capability: PeerCapability,
    pub(crate) peer_authorization: PeerAuthorization,
    /// Counts `snapshot()` calls, so a test can prove no inventory was computed before
    /// admission (#406/#881: the auth phase gates the session, so a rejected peer must
    /// trigger zero snapshots). Cloned out before the store moves into a session.
    pub(crate) snapshot_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl TestStore {
    pub(crate) fn new(
        account: [u8; 32],
        entries: impl IntoIterator<Item = ([u8; 32], Vec<u8>)>,
        local_capability: PeerCapability,
        peer_capability: PeerCapability,
    ) -> Self {
        Self {
            account,
            entries: entries.into_iter().collect(),
            local_capability,
            peer_authorization: PeerAuthorization::Granted(peer_capability),
            snapshot_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl SyncStore for TestStore {
    fn account_id(&self) -> [u8; 32] {
        self.account
    }

    fn set_serve_scope(&mut self, _scope: crate::session::ServeScope) {}

    fn snapshot(&self) -> anyhow::Result<Vec<([u8; 32], Vec<u8>)>> {
        self.snapshot_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self.entries.iter().map(|(hash, bytes)| (*hash, bytes.clone())).collect())
    }

    fn ingest(&mut self, signed_bytes: &[u8]) -> anyhow::Result<crate::session::Ingested> {
        let hash: [u8; 32] = signed_bytes[..32].try_into()?;
        match self.entries.entry(hash) {
            std::collections::hash_map::Entry::Occupied(_) =>
                Ok(crate::session::Ingested::NoChange),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(signed_bytes.to_vec());
                Ok(crate::session::Ingested::Stored)
            },
        }
    }
}

impl NodeAuth for TestStore {
    fn local_auth(&self, _local_node: &[u8; 32], _now_ms: i64) -> anyhow::Result<LocalAuth> {
        Ok(LocalAuth { binding: vec![1], capability: self.local_capability })
    }

    fn authorize(
        &self,
        _binding: &[u8],
        _remote_node: &[u8; 32],
        _now_ms: i64,
    ) -> anyhow::Result<PeerAuthorization> {
        Ok(self.peer_authorization)
    }
}

pub(crate) struct TableTestStore {
    pub(crate) auth: TestStore,
    pub(crate) supported: Vec<crate::table_wire::ManifestItem>,
    pub(crate) entries: HashMap<[u8; 32], HashMap<[u8; 32], Vec<u8>>>,
}

impl TableTestStore {
    pub(crate) fn new(
        account: [u8; 32],
        supported: Vec<crate::table_wire::ManifestItem>,
        entries: impl IntoIterator<Item = ([u8; 32], ([u8; 32], Vec<u8>))>,
    ) -> Self {
        let mut by_stream: HashMap<_, HashMap<_, _>> = HashMap::new();
        for (stream, (hash, bytes)) in entries {
            by_stream.entry(stream).or_default().insert(hash, bytes);
        }
        Self {
            auth: TestStore::new(account, [], PeerCapability::ReadWrite, PeerCapability::ReadWrite),
            supported,
            entries: by_stream,
        }
    }
}

impl TableSyncStore for TableTestStore {
    fn account_id(&self) -> [u8; 32] {
        self.auth.account
    }

    fn supported_streams(&self) -> anyhow::Result<Vec<crate::table_wire::ManifestItem>> {
        Ok(self.supported.clone())
    }

    fn validates(&self, item: &crate::table_wire::ManifestItem) -> anyhow::Result<bool> {
        Ok(self.supported.contains(item))
    }

    fn chain_page(
        &self,
        item: &crate::table_wire::ManifestItem,
        after_device: Option<[u8; 32]>,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::table_wire::ChainHead>> {
        let mut devices: Vec<_> = self
            .entries
            .get(&item.stream_id)
            .into_iter()
            .flatten()
            .map(|(hash, _)| *hash)
            .filter(|device| after_device.is_none_or(|after| *device > after))
            .collect();
        devices.sort();
        Ok(devices
            .into_iter()
            .take(limit)
            .map(|device| crate::table_wire::ChainHead {
                device_fingerprint: device,
                lamport: 0,
                entry_hash: device,
                floor: None,
            })
            .collect())
    }

    fn frontier(
        &self,
        item: &crate::table_wire::ManifestItem,
        device: [u8; 32],
    ) -> anyhow::Result<crate::table_wire::FrontierState> {
        Ok(
            if self
                .entries
                .get(&item.stream_id)
                .is_some_and(|entries| entries.contains_key(&device))
            {
                crate::table_wire::FrontierState::Accepted { lamport: 0, entry_hash: device }
            } else {
                crate::table_wire::FrontierState::Empty
            },
        )
    }

    fn entries(
        &self,
        item: &crate::table_wire::ManifestItem,
        device: [u8; 32],
        start: crate::table_session::ChainStart,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::table_session::ChainEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(bytes) =
            self.entries.get(&item.stream_id).and_then(|entries| entries.get(&device))
        else {
            return Ok(Vec::new());
        };
        let include = match start {
            crate::table_session::ChainStart::Beginning => true,
            crate::table_session::ChainStart::After { lamport, entry_hash } => {
                if lamport != 0 || entry_hash != device {
                    return Ok(Vec::new());
                }
                false
            },
            crate::table_session::ChainStart::At { lamport, entry_hash } =>
                lamport == 0 && entry_hash == device,
        };
        Ok(include
            .then(|| crate::table_session::ChainEntry {
                lamport: 0,
                entry_hash: device,
                signed_bytes: bytes.clone(),
            })
            .into_iter()
            .collect())
    }

    fn ingest(
        &mut self,
        item: &crate::table_wire::ManifestItem,
        offered: &crate::table_wire::ChainHead,
        signed_bytes: &[u8],
    ) -> anyhow::Result<crate::session::Ingested> {
        if !self.supported.contains(item) {
            return Ok(crate::session::Ingested::NoChange);
        }
        let hash: [u8; 32] = signed_bytes[..32].try_into()?;
        if hash != offered.device_fingerprint {
            return Ok(crate::session::Ingested::NoChange);
        }
        Ok(match self.entries.entry(item.stream_id).or_default().entry(hash) {
            std::collections::hash_map::Entry::Occupied(_) => crate::session::Ingested::NoChange,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(signed_bytes.to_vec());
                crate::session::Ingested::Stored
            },
        })
    }
}

impl NodeAuth for TableTestStore {
    fn local_auth(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<LocalAuth> {
        self.auth.local_auth(local_node, now_ms)
    }

    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<PeerAuthorization> {
        self.auth.authorize(binding, remote_node, now_ms)
    }
}
