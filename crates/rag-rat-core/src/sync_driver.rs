//! Shared device-sync driver for the CLI fallback and the active MCP resident host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use rag_rat_base::config::Config;
use rag_rat_base::{hash, locks, time};
use rag_rat_db::storage::IndexConnection;
use rag_rat_sync::{
    AuthPolicy, NodeAuth, OplogContentSyncStore, OplogSyncStore, PeerAuthorization, PeerCapability,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const LAST_SYNC: &str = "sync_device_last_at_ms";
const RESIDENT_HEARTBEAT: &str = "sync_resident_heartbeat_at_ms";
const RESIDENT_NUDGE: &str = "sync_resident_nudge_at_ms";
const HEARTBEAT_MAX_AGE_MS: i64 = 30_000;
const HEARTBEAT_INTERVAL_MS: i64 = HEARTBEAT_MAX_AGE_MS / 2;
const NODE_SECRET: &str = "sync_node_secret";
const DISCOVERY_ADVERTISEMENT: &str = "sync_discovery_advertisement";
const ADVERTISEMENT_REFRESH: Duration = Duration::from_secs(1);
/// Bound pre-auth peers as well as authenticated sessions: each task owns a SQLite connection
/// until the stream-idle timeout expires.
const RESIDENT_SESSION_MAX: usize = 8;
/// The most concurrent inbound sessions ONE peer (by node id) may hold. Below
/// `RESIDENT_SESSION_MAX` so no single peer monopolizes the pool, and above a legitimate peer's
/// real concurrency (dialers sync sequentially — ~1-2 in flight while a session's post-work
/// overlaps the peer's next dial), so honest use is never denied.
const RESIDENT_SESSIONS_PER_PEER_MAX: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum DeviceSyncOutcome {
    Disabled,
    Skipped,
    Deferred,
    Ran { peers: usize, ok: usize, errors: usize },
}

enum ResidentHostReady {
    Started,
    Unavailable,
}

/// The active MCP process is the sole owner of this database's endpoint and session lock.
pub struct ResidentSyncHost {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ResidentSyncHost {
    fn drop(&mut self) {
        let _ = self.shutdown.take().map(|shutdown| shutdown.send(()));
        if let Some(task) = self.task.take() {
            // Reconciliation owns bounded network waits. Reap it without holding up MCP EOF or
            // hot-upgrade teardown; the worker owns the session lock until it actually stops.
            let _ = std::thread::Builder::new().name("rag-rat-sync-reap".to_string()).spawn(
                move || {
                    let _ = task.join();
                },
            );
        }
    }
}

impl ResidentSyncHost {
    pub fn start(config: Config) -> anyhow::Result<Option<Self>> {
        let database = config.database.clone();
        // The first open follows the same migration/compatibility gate as every active MCP path;
        // a raw storage open here could create an empty database or bypass a newer-schema refusal.
        let db = crate::IndexDatabase::open_config(&config)?;
        let conn = db.connection();
        let Some(account) = rag_rat_oplog::read_local_account(conn)? else {
            return Ok(None);
        };
        let secret = node_secret(conn)?;
        let node = rag_rat_sync::node_id_from_secret(*secret);
        if !can_host(roster_capability(conn, account, &node)?) {
            return Ok(None);
        }
        drop(db);
        let relay = relay_url(&config);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let heartbeat_database = database.clone();
        let task =
            std::thread::Builder::new().name("rag-rat-sync".to_string()).spawn(move || {
                // `WriteLock` records reentrancy per thread, so acquire and release it only on the
                // worker that owns the endpoint. It stays held through the final network wait.
                let session = match locks::WriteLock::acquire_sync_session_timeout(
                    &database,
                    LOCK_TIMEOUT,
                ) {
                    Ok(Some(session)) => session,
                    Ok(None) => {
                        let _ = ready_tx.send(Ok(ResidentHostReady::Unavailable));
                        return;
                    },
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    },
                };
                let _session = session;
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
                let ready_for_runtime = ready_tx.clone();
                let result: anyhow::Result<()> = match runtime {
                    Ok(runtime) => runtime.block_on(async move {
                        let endpoint =
                            rag_rat_sync::build_endpoint(*secret, &relay).await.with_context(
                                || format!("binding the sync endpoint over relay {relay}"),
                            )?;
                        let storage = IndexConnection::open(&database)?;
                        heartbeat(storage.connection())?;
                        drop(storage);
                        ready_for_runtime
                            .send(Ok(ResidentHostReady::Started))
                            .map_err(|_| anyhow!("resident sync host startup was abandoned"))?;
                        tokio::task::LocalSet::new()
                            .run_until(resident_loop(
                                config,
                                endpoint,
                                account,
                                database,
                                shutdown_rx,
                            ))
                            .await;
                        Ok(())
                    }),
                    Err(error) => Err(error.into()),
                };
                match IndexConnection::open(&heartbeat_database) {
                    Ok(storage) => {
                        if let Err(error) =
                            rag_rat_db::meta::delete_meta(storage.connection(), RESIDENT_HEARTBEAT)
                        {
                            tracing::warn!(%error, "could not clear the resident sync heartbeat");
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "could not open the resident sync store for heartbeat cleanup");
                    },
                }
                if let Err(error) = result {
                    let _ = ready_tx.send(Err(error));
                }
            })?;
        match ready_rx.recv()? {
            Ok(ResidentHostReady::Started) =>
                Ok(Some(Self { shutdown: Some(shutdown), task: Some(task) })),
            Ok(ResidentHostReady::Unavailable) => {
                let _ = task.join();
                Ok(None)
            },
            Err(error) => {
                let _ = task.join();
                Err(error)
            },
        }
    }
}

/// Record a durable hook request and report whether a resident host has recently heartbeated.
pub fn nudge_resident_host(conn: &Connection) -> anyhow::Result<bool> {
    let now = time::now_ms();
    rag_rat_db::meta::set_meta(conn, RESIDENT_NUDGE, &now.to_string())?;
    let heartbeat = rag_rat_db::meta::read_meta(conn, RESIDENT_HEARTBEAT)?
        .and_then(|value| value.parse::<i64>().ok());
    Ok(heartbeat.is_some_and(|at| now.saturating_sub(at) <= HEARTBEAT_MAX_AGE_MS))
}

/// The short-lived CLI fallback when no resident endpoint is available.
pub fn device_sync_run(config: &Config, conn: &Connection) -> anyhow::Result<DeviceSyncOutcome> {
    let Some(account) = rag_rat_oplog::read_local_account(conn)? else {
        return Ok(DeviceSyncOutcome::Disabled);
    };
    if !sync_due(conn, config.sync.push_interval_secs)? {
        return Ok(DeviceSyncOutcome::Skipped);
    }
    let Some(_session) =
        locks::WriteLock::acquire_sync_session_timeout(&config.database, LOCK_TIMEOUT)?
    else {
        return Ok(DeviceSyncOutcome::Deferred);
    };
    if !sync_due(conn, config.sync.push_interval_secs)? {
        return Ok(DeviceSyncOutcome::Skipped);
    }
    let secret = node_secret(conn)?;
    let node = rag_rat_sync::node_id_from_secret(*secret);
    if !can_sync(roster_capability(conn, account, &node)?) {
        record_sync(conn)?;
        return Ok(DeviceSyncOutcome::Disabled);
    }
    let relay = relay_url(config);
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let outcome = runtime.block_on(async {
        let endpoint = rag_rat_sync::build_endpoint(*secret, &relay)
            .await
            .with_context(|| format!("binding the sync endpoint over relay {relay}"))?;
        reconcile(config, conn, &endpoint, account).await
    });
    record_sync(conn)?;
    outcome.map(|(peers, ok, errors)| DeviceSyncOutcome::Ran { peers, ok, errors })
}

async fn resident_loop(
    config: Config,
    endpoint: iroh::Endpoint,
    account: rag_rat_oplog::AccountId,
    database: PathBuf,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let accept = tokio::task::spawn_local(accept_loop(endpoint.clone(), account, database.clone()));
    // Advertising owns a separate timer and short-lived DB handles. Inbound sessions stay on their
    // own task, so a seal retry cannot cancel or delay a session already in flight.
    let advertiser = tokio::task::spawn_local(advertise_host(
        config.clone(),
        endpoint.clone(),
        database.clone(),
    ));
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    let mut handled_nudge = 0;
    let mut last_heartbeat = 0;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = poll.tick() => {},
        }
        let result = async {
            let storage = IndexConnection::open(&database)?;
            let conn = storage.connection();
            let nudge = rag_rat_db::meta::read_meta(conn, RESIDENT_NUDGE)?
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or_default();
            let now = time::now_ms();
            if last_heartbeat == 0
                || now < last_heartbeat
                || now - last_heartbeat >= HEARTBEAT_INTERVAL_MS
            {
                heartbeat(conn)?;
                last_heartbeat = now;
            }
            if nudge <= handled_nudge && !sync_due(conn, config.sync.push_interval_secs)? {
                return anyhow::Ok(());
            }
            let outcome = reconcile(&config, conn, &endpoint, account).await;
            record_sync(conn)?;
            handled_nudge = handled_nudge.max(nudge);
            outcome.map(|_| ())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, "resident device sync failed; the next cadence retries");
        }
    }
    accept.abort();
    advertiser.abort();
}

/// Persisted local state for one serving endpoint's announcement. The service appends rather than
/// replaces, so the exact sealed bytes and their possible liveness must survive a restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedAdvertisement {
    tag: [u8; 32],
    node: [u8; 32],
    service: [u8; 32],
    relay: String,
    roster_stamp: Option<[u8; 32]>,
    envelope: Option<Vec<u8>>,
    published_at_ms: Option<i64>,
    ttl_seconds: u32,
}

impl PersistedAdvertisement {
    fn matches(
        &self,
        tag: &[u8; 32],
        node: &[u8; 32],
        service: &[u8; 32],
        relay: &str,
        stamp: &Option<[u8; 32]>,
    ) -> bool {
        self.tag == *tag
            && self.node == *node
            && self.service == *service
            && self.relay == relay
            && self.roster_stamp == *stamp
    }

    fn live(&self, now_ms: i64) -> bool {
        self.envelope.is_some()
            && self.published_at_ms.is_some_and(|published_at| {
                // A backwards wall-clock step must not turn one possibly-live publication into a
                // burst of appends. The next normal clock reading renews it.
                published_at > now_ms
                    || now_ms.saturating_sub(published_at)
                        < i64::try_from(
                            rag_rat_sync::discovery::renew_after(self.ttl_seconds).as_millis(),
                        )
                        .unwrap_or(i64::MAX)
            })
    }
}

struct Publication {
    tag: [u8; 32],
    node: [u8; 32],
    service: [u8; 32],
    relay: String,
    envelope: Vec<u8>,
    ttl_seconds: u32,
}

struct RefusedPublication {
    envelope: Vec<u8>,
    attempted_at: tokio::time::Instant,
}

fn retry_is_due(
    last_attempt: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    retry_after: Duration,
) -> bool {
    last_attempt.is_none_or(|attempted_at| now.duration_since(attempted_at) >= retry_after)
}

fn refused_publication_is_due(
    refusal: Option<&RefusedPublication>,
    envelope: &[u8],
    now: tokio::time::Instant,
    retry_after: Duration,
) -> bool {
    !matches!(
        refusal,
        Some(refusal)
            if refusal.envelope == envelope
                && !retry_is_due(Some(refusal.attempted_at), now, retry_after)
    )
}

/// Whether `account` is a published public knowledge base — served under `AuthPolicy::PublicRead`
/// (anonymous read) rather than `Closed`. True iff the account is fully public (no private stream)
/// AND owns at least one stream, so a vacuously-fully-public FRESH/empty account is NOT exposed;
/// only a deliberate `sync publish` (which refuses a non-fully-public account and ensures the
/// public stream) satisfies both. Evaluated PER-CONNECTION by the serve loops so a node published
/// while the host runs starts serving public without a restart; the store's own
/// `account_is_fully_public` snapshot guard remains the fail-closed backstop.
pub fn account_is_public_kb(
    conn: &Connection,
    account: rag_rat_oplog::AccountId,
) -> anyhow::Result<bool> {
    if !rag_rat_oplog::account_is_fully_public(conn, account)? {
        return Ok(false);
    }
    if !rag_rat_oplog::owned_streams_for_account(conn, account)?.is_empty() {
        return Ok(true);
    }
    // A granted CONTRIBUTOR owns no stream at all (#1164) — it authors onto the owner's — so the
    // owns-a-stream test alone would serve it `Closed` and nothing could ever pull its account log.
    // That breaks the very direction contribution needs: content is offered by AUTHOR, so the owner
    // collects a contributor's memories by syncing the CONTRIBUTOR's account, which requires the
    // contributor to be servable. Holding an effective Writer grant is the same kind of deliberate
    // act as publishing — it is not the vacuously-public fresh account the stream test exists to
    // keep unexposed.
    //
    // The exposure is stated rather than implied: this makes the contributor's account log readable
    // by ANY dialer, not just the owner, because public admission is anonymous. Grants imply a
    // PublicRead stream, so the content it authored is public regardless; what this adds is the
    // contributor's own roster metadata.
    rag_rat_oplog::account_holds_effective_writer_grant(conn, account)
}

/// Maintain a serving host's discovery announcement independently of its inbound session loop.
///
/// The record in `index_meta` is reused only for the same endpoint identity, account tag, and
/// roster stamp. This preserves byte identity across a restart while making a roster move reseal
/// promptly. The timer retries an initial or replacement seal even when no peer ever connects.
pub async fn advertise_host(config: Config, endpoint: iroh::Endpoint, database: PathBuf) {
    if !config.sync.discovery || !config.sync.discoverable {
        return;
    }
    let relay = relay_url(&config);
    let Some(service) = discovery_addr(&config, &relay) else {
        return;
    };
    let ttl_seconds = rag_rat_sync::discovery::publish_ttl_seconds(config.sync.push_interval_secs);
    let retry_after = rag_rat_sync::discovery::retry_after_refusal(ttl_seconds);
    let node = *endpoint.id().as_bytes();
    let service_node = *service.id.as_bytes();
    let mut refresh = tokio::time::interval(ADVERTISEMENT_REFRESH);
    let mut last_prepare_failure = None;
    let mut last_refused = None;
    loop {
        refresh.tick().await;
        let now = tokio::time::Instant::now();
        if !retry_is_due(last_prepare_failure, now, retry_after) {
            continue;
        }
        let publication = match prepare_advertisement(
            &database,
            &node,
            &service_node,
            &relay,
            time::now_ms(),
            ttl_seconds,
        ) {
            Ok(Some(publication)) => {
                last_prepare_failure = None;
                publication
            },
            Ok(None) => {
                last_prepare_failure = None;
                continue;
            },
            Err(error) => {
                last_prepare_failure = Some(now);
                tracing::warn!(%error, "could not prepare this host's discovery announcement");
                continue;
            },
        };
        let attempted_at = tokio::time::Instant::now();
        if !refused_publication_is_due(
            last_refused.as_ref(),
            &publication.envelope,
            attempted_at,
            retry_after,
        ) {
            continue;
        }
        let attempted_at_ms = time::now_ms();
        let outcome =
            rag_rat_sync::discovery::exchange(rag_rat_sync::discovery::DiscoveryExchange {
                endpoint: &endpoint,
                service: service.clone(),
                tag: publication.tag,
                fetch: false,
                publish: Some(&publication.envelope),
                ttl_seconds,
            })
            .await;
        if rag_rat_sync::discovery::records_liveness(outcome.publish)
            && let Err(error) =
                record_advertisement_liveness(&database, &publication, attempted_at_ms)
        {
            tracing::warn!(%error, "could not persist this host's discovery liveness");
        }
        last_refused = match outcome.publish {
            rag_rat_sync::discovery::PublishState::Refused =>
                Some(RefusedPublication { envelope: publication.envelope, attempted_at }),
            _ => None,
        };
        match outcome.degraded {
            Some(reason) => tracing::warn!(reason, "advertising this host degraded"),
            None => tracing::debug!(state = ?outcome.publish, "advertised this host"),
        }
    }
}

fn prepare_advertisement(
    database: &Path,
    node: &[u8; 32],
    service: &[u8; 32],
    relay: &str,
    now_ms: i64,
    ttl_seconds: u32,
) -> anyhow::Result<Option<Publication>> {
    let storage = IndexConnection::open(database)?;
    let conn = storage.connection();
    let Some(secret) = rag_rat_sync::discovery::discovery_secret(conn)? else {
        return Ok(None);
    };
    let tag = rag_rat_sync::discovery::account_tag(&secret);
    let stamp = rag_rat_oplog::discovery::roster_stamp(conn)?;
    let persisted = read_advertisement(conn)?;
    let current = persisted.filter(|record| record.matches(&tag, node, service, relay, &stamp));
    let record = match current {
        Some(record) => record,
        None => {
            let envelope = seal_advertisement(conn, &tag, node)?;
            let record = PersistedAdvertisement {
                tag,
                node: *node,
                service: *service,
                relay: relay.to_owned(),
                roster_stamp: stamp,
                envelope,
                published_at_ms: None,
                ttl_seconds,
            };
            write_advertisement(conn, &record)?;
            record
        },
    };
    if record.live(now_ms) {
        return Ok(None);
    }
    Ok(record.envelope.map(|envelope| Publication {
        tag,
        node: *node,
        service: *service,
        relay: relay.to_owned(),
        envelope,
        ttl_seconds,
    }))
}

fn record_advertisement_liveness(
    database: &Path,
    publication: &Publication,
    attempted_at_ms: i64,
) -> anyhow::Result<()> {
    let storage = IndexConnection::open(database)?;
    let conn = storage.connection();
    let stamp = rag_rat_oplog::discovery::roster_stamp(conn)?;
    let Some(mut record) = read_advertisement(conn)? else {
        return Ok(());
    };
    // Never make a publish from a roster that moved during the network exchange look current.
    if !record.matches(
        &publication.tag,
        &publication.node,
        &publication.service,
        &publication.relay,
        &stamp,
    ) || record.envelope.as_deref() != Some(publication.envelope.as_slice())
    {
        return Ok(());
    }
    record.published_at_ms = Some(attempted_at_ms);
    record.ttl_seconds = publication.ttl_seconds;
    write_advertisement(conn, &record)
}

fn seal_advertisement(
    conn: &Connection,
    tag: &[u8; 32],
    node: &[u8; 32],
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(sealed) = rag_rat_oplog::discovery::seal_discovery_announcement(conn, tag, node)?
    else {
        return Ok(None);
    };
    if sealed.recipients <= 1 {
        return Ok(None);
    }
    if sealed.bytes.len() > rag_rat_sync::discovery::MAX_ANNOUNCEMENT_BYTES {
        tracing::warn!(
            recipients = sealed.recipients,
            bytes = sealed.bytes.len(),
            max_bytes = rag_rat_sync::discovery::MAX_ANNOUNCEMENT_BYTES,
            "not advertising: this account's roster is too large to seal into one announcement"
        );
        return Ok(None);
    }
    Ok(Some(sealed.bytes))
}

fn read_advertisement(conn: &Connection) -> anyhow::Result<Option<PersistedAdvertisement>> {
    let Some(encoded) = rag_rat_db::meta::read_meta(conn, DISCOVERY_ADVERTISEMENT)? else {
        return Ok(None);
    };
    match serde_json::from_str(&encoded) {
        Ok(record) => Ok(Some(record)),
        Err(error) => {
            tracing::warn!(%error, "ignoring a malformed persisted discovery advertisement");
            Ok(None)
        },
    }
}

fn write_advertisement(conn: &Connection, record: &PersistedAdvertisement) -> anyhow::Result<()> {
    rag_rat_db::meta::set_meta(conn, DISCOVERY_ADVERTISEMENT, &serde_json::to_string(record)?)
}

/// Per-peer concurrent-session fairness for the resident accept loop: bounds how many in-flight
/// sessions one node id holds, so a stuck or greedy fixed-identity peer can't occupy every
/// `RESIDENT_SESSION_MAX` slot and starve others. Honest-peer FAIRNESS, not anti-Sybil — a node id
/// is mintable, so id rotation evades this; that flood is shed upstream by the Sybil-proof global
/// accept-rate limit (`GlobalAcceptRateLimiter`). This is a DIFFERENT map from the per-id rate
/// limit that comment rejects: it is keyed only by ids with a LIVE session (entries removed at
/// zero, bounded by `RESIDENT_SESSION_MAX`), not by ids-ever-seen — so it needs no eviction and
/// cannot be a memory target.
#[derive(Clone, Default)]
struct PerPeerSessionLimiter {
    in_flight: Arc<Mutex<HashMap<[u8; 32], usize>>>,
}

impl PerPeerSessionLimiter {
    /// Reserve a session slot for `peer`, or `None` if it already holds `max`. The returned guard
    /// releases the slot on drop — task completion, including a panicked task's unwind.
    fn try_acquire(&self, peer: [u8; 32], max: usize) -> Option<PerPeerSessionSlot> {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|poison| poison.into_inner());
        if in_flight.get(&peer).copied().unwrap_or(0) >= max {
            return None;
        }
        *in_flight.entry(peer).or_insert(0) += 1;
        Some(PerPeerSessionSlot { in_flight: Arc::clone(&self.in_flight), peer })
    }
}

/// RAII release of one [`PerPeerSessionLimiter`] slot.
struct PerPeerSessionSlot {
    in_flight: Arc<Mutex<HashMap<[u8; 32], usize>>>,
    peer: [u8; 32],
}

impl Drop for PerPeerSessionSlot {
    fn drop(&mut self) {
        // Poison-tolerant: this runs during task teardown, possibly a panic unwind; a `.unwrap()`
        // on a poisoned lock here would escalate to an abort.
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(count) = in_flight.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                in_flight.remove(&self.peer);
            }
        }
    }
}

async fn accept_loop(
    endpoint: iroh::Endpoint,
    account: rag_rat_oplog::AccountId,
    database: PathBuf,
) {
    let sessions = Arc::new(tokio::sync::Semaphore::new(RESIDENT_SESSION_MAX));
    // Global inbound accept-rate limit: refuses a connection flood BEFORE the handshake, regardless
    // of peer id (Sybil-resistant). Loop-owned, so no locking.
    let mut accept_rate = rag_rat_sync::GlobalAcceptRateLimiter::new();
    // Per-peer concurrent-session fairness: one node id may not hold every session slot.
    let per_peer = PerPeerSessionLimiter::default();
    // Global egress byte cap shared across all concurrent session tasks (an `Arc<Mutex>` — unlike
    // the sequential accept limiter above, sessions run in parallel): bounds total data served
    // so a peer cannot drain the host by re-pulling.
    let egress = Arc::new(Mutex::new(rag_rat_sync::GlobalEgressLimiter::new()));
    loop {
        let connection = match rag_rat_sync::accept_connection_within_rate(
            &endpoint,
            &mut accept_rate,
            time::now_ms,
        )
        .await
        {
            Ok(Some(connection)) => connection,
            // Refused by the accept-rate limit before the handshake — nothing served, take the
            // next.
            Ok(None) => continue,
            Err(error) if endpoint.is_closed() => {
                tracing::warn!(%error, "resident sync endpoint closed");
                return;
            },
            Err(error) => {
                tracing::warn!(%error, "resident sync accept failed");
                continue;
            },
        };
        // Per-peer fairness BEFORE the global permit, so an over-cap peer never even transiently
        // consumes one of the shared slots. `remote_id()` is the iroh-authenticated peer node id.
        let Some(peer_slot) = per_peer
            .try_acquire(*connection.remote_id().as_bytes(), RESIDENT_SESSIONS_PER_PEER_MAX)
        else {
            connection.close(0u32.into(), b"peer-session-limit");
            continue;
        };
        let Ok(permit) = Arc::clone(&sessions).try_acquire_owned() else {
            // Do not queue unauthenticated peers behind the session limit. Their connections can
            // wait out the stream timeout otherwise, consuming endpoint and OS resources.
            connection.close(0u32.into(), b"session-limit");
            continue; // `peer_slot` drops here, releasing this peer's reservation.
        };
        let database = database.clone();
        let node = *endpoint.id().as_bytes();
        let egress = Arc::clone(&egress);
        tokio::task::spawn_local(async move {
            let _permit = permit;
            // Held to task end so this peer's slot is reserved for the whole session, then released
            // on drop (normal completion or a panic unwind).
            let _peer_slot = peer_slot;
            let result = async {
                let storage = IndexConnection::open(&database)?;
                let conn = storage.connection();
                let mut account_store = OplogSyncStore::new(conn, account, time::now_ms);
                let mut content_store = OplogContentSyncStore::new(conn, account, time::now_ms);
                // A published public-KB account is served PublicRead (anonymous read); every other
                // account stays Closed. Derived per-connection so a mid-run `sync publish` takes
                // effect without a restart.
                let policy = if account_is_public_kb(conn, account)? {
                    AuthPolicy::PublicRead
                } else {
                    AuthPolicy::Closed
                };
                let (alpn, report) = rag_rat_sync::dispatch_connection(
                    connection,
                    node,
                    &mut account_store,
                    &mut content_store,
                    policy,
                    time::now_ms,
                    Some(egress),
                )
                .await?;
                if alpn.as_slice() == rag_rat_sync::CONTENT_SYNC_ALPN {
                    crate::drain_synced_memory(conn)?;
                } else if alpn.as_slice() == rag_rat_sync::TABLE_SYNC_ALPN {
                    // The resident host holds the index open, so the on-open re-resolution never
                    // re-fires for anchors pushed here — resolve them at the session's settle
                    // point.
                    crate::resolve_synced_distill_anchors(conn)?;
                }
                anyhow::Ok(report)
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(%error, "resident inbound sync session failed");
            }
        });
    }
}

async fn reconcile(
    config: &Config,
    conn: &Connection,
    endpoint: &iroh::Endpoint,
    account: rag_rat_oplog::AccountId,
) -> anyhow::Result<(usize, usize, usize)> {
    let relay = relay_url(config);
    let tag = rag_rat_sync::discovery::discovery_secret(conn)?
        .map(|secret| rag_rat_sync::discovery::account_tag(&secret));
    let discovery = discovery_addr(config, &relay);
    let resolved = rag_rat_sync::discover_peers(
        &config.sync.server_peers,
        &relay,
        tag.zip(discovery).map(|(tag, service)| rag_rat_sync::discovery::DiscoveryExchange {
            endpoint,
            service,
            tag,
            fetch: true,
            publish: None,
            ttl_seconds: rag_rat_sync::discovery::publish_ttl_seconds(
                config.sync.push_interval_secs,
            ),
        }),
        &|payload| {
            tag.and_then(|tag| {
                rag_rat_oplog::discovery::open_discovery_announcement(conn, &tag, payload)
                    .ok()
                    .flatten()
            })
        },
    )
    .await;
    let mut reached = vec![false; resolved.peers.len()];
    for (index, (peer, address)) in resolved.peers.iter().enumerate() {
        let mut store = OplogSyncStore::new(conn, account, time::now_ms);
        match rag_rat_sync::connect_and_reconcile(
            endpoint,
            address.clone(),
            rag_rat_sync::SYNC_ALPN,
            &mut store,
            AuthPolicy::Closed,
            time::now_ms,
            rag_rat_sync::MAX_RECONCILE_ROUNDS,
        )
        .await
        {
            Ok(report) if report.converged => reached[index] = true,
            Ok(_) => tracing::warn!(peer, "device sync account reconciliation did not converge"),
            Err(error) => tracing::warn!(peer, %error, "device sync account reconciliation failed"),
        }
    }
    ensure_founder_incarnations(conn)?;
    // A newly authored founder incarnation must reach peers before their table manifests run.
    for (index, (peer, address)) in resolved.peers.iter().enumerate() {
        if !reached[index] {
            continue;
        }
        let mut store = OplogSyncStore::new(conn, account, time::now_ms);
        match rag_rat_sync::connect_and_reconcile(
            endpoint,
            address.clone(),
            rag_rat_sync::SYNC_ALPN,
            &mut store,
            AuthPolicy::Closed,
            time::now_ms,
            rag_rat_sync::MAX_RECONCILE_ROUNDS,
        )
        .await
        {
            Ok(report) if report.converged => {},
            Ok(_) => {
                tracing::warn!(peer, "device sync incarnation propagation did not converge");
                reached[index] = false;
            },
            Err(error) => {
                tracing::warn!(peer, %error, "device sync incarnation propagation failed");
                reached[index] = false;
            },
        }
    }
    for (index, (peer, address)) in resolved.peers.iter().enumerate() {
        if !reached[index] {
            continue;
        }
        let mut content = OplogContentSyncStore::new(conn, account, time::now_ms);
        if let Err(error) = rag_rat_sync::connect_and_reconcile(
            endpoint,
            address.clone(),
            rag_rat_sync::CONTENT_SYNC_ALPN,
            &mut content,
            AuthPolicy::Closed,
            time::now_ms,
            rag_rat_sync::MAX_RECONCILE_ROUNDS,
        )
        .await
        {
            tracing::warn!(peer, %error, "device sync content reconciliation failed");
        }
        crate::drain_synced_memory(conn)?;
        let mut tables = rag_rat_sync::OplogTableSyncStore::new(conn, account, time::now_ms);
        if tables.has_streams()?
            && let Err(error) = rag_rat_sync::connect_and_table_reconcile(
                endpoint,
                address.clone(),
                &mut tables,
                time::now_ms,
                rag_rat_sync::MAX_RECONCILE_ROUNDS,
            )
            .await
        {
            tracing::warn!(peer, %error, "device sync table reconciliation failed");
        }
    }
    // Resolve any anchors this run's table reconciliation pulled against the local index, so they
    // surface as drive-by without waiting for the next index open (idempotent when nothing
    // changed).
    crate::resolve_synced_distill_anchors(conn)?;
    let ok = reached.iter().filter(|reached| **reached).count();
    let peers = reached.len() + resolved.unresolved_configured;
    Ok((peers, ok, peers - ok))
}

pub fn relay_url(config: &Config) -> String {
    std::env::var("RAG_RAT_SYNC_RELAY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| config.sync.relay_url.clone())
}

fn discovery_addr(config: &Config, relay: &str) -> Option<rag_rat_sync::EndpointAddr> {
    if !config.sync.discovery {
        return None;
    }
    let node = std::env::var("RAG_RAT_SYNC_DISCOVERY_NODE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| config.sync.discovery_node_id.clone());
    rag_rat_sync::peer_addr(&node, relay).map_err(|error| {
        tracing::warn!(%error, "skipping peer discovery: configured node id is invalid")
    }).ok()
}

fn sync_due(conn: &Connection, interval_secs: u64) -> anyhow::Result<bool> {
    if interval_secs == 0 {
        return Ok(true);
    }
    let Some(last) =
        rag_rat_db::meta::read_meta(conn, LAST_SYNC)?.and_then(|value| value.parse::<i64>().ok())
    else {
        return Ok(true);
    };
    let now = time::now_ms();
    Ok(last > now
        || now - last >= i64::try_from(interval_secs).unwrap_or(i64::MAX).saturating_mul(1000))
}

fn record_sync(conn: &Connection) -> anyhow::Result<()> {
    rag_rat_db::meta::set_meta(conn, LAST_SYNC, &time::now_ms().to_string())
}

fn heartbeat(conn: &Connection) -> anyhow::Result<()> {
    rag_rat_db::meta::set_meta(conn, RESIDENT_HEARTBEAT, &time::now_ms().to_string())
}

fn roster_capability(
    conn: &Connection,
    account: rag_rat_oplog::AccountId,
    node: &[u8; 32],
) -> anyhow::Result<Option<PeerCapability>> {
    let store = OplogSyncStore::new(conn, account, time::now_ms);
    let now = time::now_ms();
    let local = store.local_auth(node, now)?;
    match store.authorize(&local.binding, node, now)? {
        PeerAuthorization::Granted(capability) => Ok(Some(capability)),
        PeerAuthorization::Rejected | PeerAuthorization::Unavailable => Ok(None),
    }
}

fn can_sync(capability: Option<PeerCapability>) -> bool {
    capability.is_some()
}

fn can_host(capability: Option<PeerCapability>) -> bool {
    matches!(capability, Some(PeerCapability::ReadWrite))
}

fn ensure_founder_incarnations(conn: &Connection) -> anyhow::Result<()> {
    for repo_id in rag_rat_db::schema::real_repo_ids(conn)? {
        rag_rat_oplog::ensure_repo_incarnation(conn, &repo_id, time::now_ms())?;
    }
    Ok(())
}

fn node_secret(conn: &Connection) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    if let Some(stored) = rag_rat_db::meta::read_meta(conn, NODE_SECRET)? {
        return decode_secret(&stored);
    }
    let mut fresh = Zeroizing::new([0u8; 32]);
    getrandom::fill(fresh.as_mut_slice())
        .map_err(|error| anyhow!("OS CSPRNG unavailable to mint the sync node key: {error}"))?;
    conn.execute("INSERT OR IGNORE INTO index_meta(key, value) VALUES (?1, ?2)", params![
        NODE_SECRET,
        hash::hex_lower(fresh.as_slice())
    ])?;
    decode_secret(
        &rag_rat_db::meta::read_meta(conn, NODE_SECRET)?
            .context("sync node secret missing after mint")?,
    )
}

fn decode_secret(encoded: &str) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let encoded = encoded.trim();
    if encoded.len() != 64 {
        return Err(anyhow!(
            "persisted sync node secret is {} hex chars, expected 64",
            encoded.len()
        ));
    }
    let mut secret = Zeroizing::new([0; 32]);
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        secret[index] = u8::from_str_radix(std::str::from_utf8(pair)?, 16)
            .map_err(|_| anyhow!("persisted sync node secret is not valid hex"))?;
    }
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use rag_rat_base::config::Config;
    use rusqlite::Connection;

    use super::{
        DISCOVERY_ADVERTISEMENT, DeviceSyncOutcome, PerPeerSessionLimiter, PersistedAdvertisement,
        RESIDENT_NUDGE, RefusedPublication, account_is_public_kb, can_host, can_sync,
        device_sync_run, nudge_resident_host, read_advertisement, refused_publication_is_due,
        retry_is_due, write_advertisement,
    };

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        conn
    }

    fn own_stream(conn: &Connection, mode: rag_rat_oplog::AccessMode) {
        use rusqlite::{Transaction, TransactionBehavior};
        rag_rat_oplog::local_account(conn, 1_000).unwrap();
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(&tx, "repo-a", mode, 1_000).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn account_is_public_kb_only_for_a_published_fully_public_account() {
        // A minted-but-empty account (no owned stream) is NOT served public — else a fresh node
        // would expose itself vacuously.
        let empty = schema_conn();
        let empty_account = rag_rat_oplog::local_account(&empty, 1_000).unwrap();
        assert!(!account_is_public_kb(&empty, empty_account).unwrap());

        // A private stream is not fully public.
        let private = schema_conn();
        own_stream(&private, rag_rat_oplog::AccessMode::Private);
        let private_account = rag_rat_oplog::local_account(&private, 1_000).unwrap();
        assert!(!account_is_public_kb(&private, private_account).unwrap());

        // A published account (public stream, fully public) IS served public.
        let public = schema_conn();
        own_stream(&public, rag_rat_oplog::AccessMode::PublicRead);
        let public_account = rag_rat_oplog::local_account(&public, 1_000).unwrap();
        assert!(account_is_public_kb(&public, public_account).unwrap());
    }

    /// A granted CONTRIBUTOR owns no stream (#1164), so the owns-a-stream test alone would serve it
    /// `Closed` and nothing could pull its account log — breaking the direction contribution needs,
    /// since content is offered by AUTHOR and the owner collects a contributor's memories by
    /// syncing the CONTRIBUTOR's account. Holding an effective Writer grant is the deliberate act
    /// that distinguishes it from the vacuously-public fresh account the stream test guards.
    #[test]
    fn a_grant_holding_contributor_is_servable_even_though_it_owns_no_stream() {
        let contributor = schema_conn();
        let account = rag_rat_oplog::local_account(&contributor, 1_000).unwrap();
        // Owns nothing: today's rule refuses it.
        assert!(!account_is_public_kb(&contributor, account).unwrap());

        // The owner's grant, as it lands after the contributor syncs the owner's log.
        contributor
            .execute(
                "INSERT INTO account_stream_grants(
                     owner_account_id, grant_id, stream_id, grantee_account_id, role, effective_at)
                 VALUES (?1, ?2, ?3, ?4, 'writer', 1000)",
                rusqlite::params![
                    [0xAAu8; 32].as_slice(),
                    [0xBBu8; 32].as_slice(),
                    [0xCCu8; 32].as_slice(),
                    account.to_bytes().as_slice(),
                ],
            )
            .unwrap();
        assert!(
            account_is_public_kb(&contributor, account).unwrap(),
            "a contributor holding an effective writer grant is servable",
        );

        // A CLOSED grant is not: a revoked contributor stops being servable on that basis alone.
        contributor.execute("UPDATE account_stream_grants SET closed_at = 2000", []).unwrap();
        assert!(
            !account_is_public_kb(&contributor, account).unwrap(),
            "a revoked grant no longer makes the account servable",
        );
    }

    #[test]
    fn nudge_is_durable_when_no_resident_host_is_live() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();

        assert!(!nudge_resident_host(&conn).unwrap());
        let nudge: String = conn
            .query_row("SELECT value FROM index_meta WHERE key = ?1", [RESIDENT_NUDGE], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(nudge.parse::<i64>().is_ok(), "the hook request survives until a host observes it");
    }

    #[test]
    fn fallback_does_not_mint_or_bind_without_an_account() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        let config = Config::minimal_for_database(
            PathBuf::from("/nonexistent/sync.sqlite"),
            PathBuf::from("/nonexistent"),
        );

        assert_eq!(device_sync_run(&config, &conn).unwrap(), DeviceSyncOutcome::Disabled);
    }

    #[test]
    fn only_writers_can_host_while_read_only_devices_can_dial() {
        assert!(can_host(Some(rag_rat_sync::PeerCapability::ReadWrite)));
        assert!(!can_host(Some(rag_rat_sync::PeerCapability::ReadOnly)));
        assert!(can_sync(Some(rag_rat_sync::PeerCapability::ReadOnly)));
    }

    #[test]
    fn persisted_advertisement_matches_only_the_same_endpoint_service_relay_tag_and_roster() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        let record = PersistedAdvertisement {
            tag: [1; 32],
            node: [2; 32],
            service: [3; 32],
            relay: "https://relay.one".to_owned(),
            roster_stamp: Some([3; 32]),
            envelope: Some(vec![4, 5, 6]),
            published_at_ms: Some(7),
            ttl_seconds: 60,
        };

        write_advertisement(&conn, &record).unwrap();
        let restored = read_advertisement(&conn).unwrap().expect("the record was persisted");
        assert_eq!(restored, record, "the exact sealed envelope survives a host restart");
        assert!(restored.matches(
            &[1; 32],
            &[2; 32],
            &[3; 32],
            "https://relay.one",
            &Some([3; 32]),
        ));
        assert!(!restored.matches(
            &[9; 32],
            &[2; 32],
            &[3; 32],
            "https://relay.one",
            &Some([3; 32]),
        ));
        assert!(!restored.matches(
            &[1; 32],
            &[9; 32],
            &[3; 32],
            "https://relay.one",
            &Some([3; 32]),
        ));
        assert!(!restored.matches(
            &[1; 32],
            &[2; 32],
            &[9; 32],
            "https://relay.one",
            &Some([3; 32]),
        ));
        assert!(!restored.matches(
            &[1; 32],
            &[2; 32],
            &[3; 32],
            "https://relay.two",
            &Some([3; 32]),
        ));
        assert!(!restored.matches(
            &[1; 32],
            &[2; 32],
            &[3; 32],
            "https://relay.one",
            &Some([9; 32]),
        ));
        assert!(
            rag_rat_db::meta::read_meta(&conn, DISCOVERY_ADVERTISEMENT).unwrap().is_some(),
            "the controller stores its state in index_meta"
        );
    }

    #[test]
    fn a_restart_reuses_matching_liveness_until_renewal_is_due() {
        let record = PersistedAdvertisement {
            tag: [1; 32],
            node: [2; 32],
            service: [3; 32],
            relay: "https://relay.one".to_owned(),
            roster_stamp: Some([3; 32]),
            envelope: Some(vec![4, 5, 6]),
            published_at_ms: Some(1_000),
            ttl_seconds: 60,
        };
        assert!(
            record.live(30_999),
            "a restarted host keeps using its still-live byte-identical envelope"
        );
        assert!(
            !record.live(31_000),
            "the first renewal is still due at the established half-life"
        );
    }

    #[test]
    fn a_refused_advertisement_uses_the_ttl_derived_retry_delay() {
        let retry_after = rag_rat_sync::discovery::retry_after_refusal(60);
        assert_eq!(retry_after, Duration::from_millis(7_500));

        let attempted_at = tokio::time::Instant::now();
        let refusal = RefusedPublication { envelope: vec![1, 2, 3], attempted_at };
        assert!(
            !refused_publication_is_due(
                Some(&refusal),
                &[1, 2, 3],
                attempted_at + retry_after - Duration::from_millis(1),
                retry_after,
            ),
            "the fine controller timer must not turn a refusal into a request per second"
        );
        assert!(refused_publication_is_due(
            Some(&refusal),
            &[1, 2, 3],
            attempted_at + retry_after,
            retry_after,
        ));
        assert!(
            refused_publication_is_due(Some(&refusal), &[9], attempted_at, retry_after),
            "a roster reseal is a new envelope and should not wait behind an old refusal"
        );
    }

    #[test]
    fn a_preparation_error_uses_the_ttl_derived_retry_delay() {
        let retry_after = rag_rat_sync::discovery::retry_after_refusal(60);
        let attempted_at = tokio::time::Instant::now();
        assert!(!retry_is_due(
            Some(attempted_at),
            attempted_at + retry_after - Duration::from_millis(1),
            retry_after,
        ));
        assert!(retry_is_due(Some(attempted_at), attempted_at + retry_after, retry_after,));
    }

    const PEER_A: [u8; 32] = [0xa; 32];
    const PEER_B: [u8; 32] = [0xb; 32];

    #[test]
    fn per_peer_limiter_admits_up_to_max_then_denies() {
        let limiter = PerPeerSessionLimiter::default();
        let slots: Vec<_> = (0..3).map(|_| limiter.try_acquire(PEER_A, 3)).collect();
        assert!(slots.iter().all(Option::is_some), "the first `max` slots are admitted");
        assert!(limiter.try_acquire(PEER_A, 3).is_none(), "the slot past `max` is denied");
        // A denied acquire must not mutate the map (no phantom entry, count still exactly `max`).
        let map = limiter.in_flight.lock().unwrap();
        assert_eq!(map.len(), 1, "denial adds no entry");
        assert_eq!(map.get(&PEER_A).copied(), Some(3), "denial does not inflate the count");
    }

    #[test]
    fn per_peer_limiter_releases_and_prunes_on_slot_drop() {
        let limiter = PerPeerSessionLimiter::default();
        let a = limiter.try_acquire(PEER_A, 1).expect("first slot");
        assert!(limiter.try_acquire(PEER_A, 1).is_none(), "at capacity");
        drop(a);
        assert!(
            !limiter.in_flight.lock().unwrap().contains_key(&PEER_A),
            "the entry is removed at zero — the map holds only live sessions",
        );
        assert!(limiter.try_acquire(PEER_A, 1).is_some(), "the released slot is available again");
    }

    #[test]
    fn per_peer_limiter_is_independent_per_peer() {
        let limiter = PerPeerSessionLimiter::default();
        let _a = limiter.try_acquire(PEER_A, 1).expect("A slot");
        assert!(limiter.try_acquire(PEER_A, 1).is_none(), "A is at capacity");
        assert!(limiter.try_acquire(PEER_B, 1).is_some(), "B has its own independent cap");
    }

    #[test]
    fn per_peer_slot_releases_even_when_dropped_during_unwind() {
        let limiter = PerPeerSessionLimiter::default();
        let taken = limiter.clone();
        // A panic while the slot is in scope must still run its Drop (release), not leak the count.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = taken.try_acquire(PEER_A, 1).expect("slot");
            panic!("session task panicked mid-flight");
        }));
        assert!(
            limiter.try_acquire(PEER_A, 1).is_some(),
            "the slot was released on the panic unwind, not leaked",
        );
    }
}
