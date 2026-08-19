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
    // collects a contributor's memories by syncing the CONTRIBUTOR's account.
    //
    // Ask the NARROW question — "is there a live grant for a stream this store actually contributes
    // to" — not "does this account hold any grant anywhere". Two reasons, and both matter:
    //
    // * COST. This runs per inbound connection, BEFORE authentication. A grantee-leading scan of
    //   `account_stream_grants` is unindexed (the index is `(owner, stream, grantee)`), so a store
    //   that has synced many account logs would do unbounded work for every unauthenticated dial.
    //   Resolving the owner and stream first makes each lookup an indexed point query, and the
    //   number of them is the number of contributing repos — a handful.
    // * PRECISION. A stale grant this store never uses should not expose it.
    //
    // The grant's stream must be PublicRead, checked here and not assumed: the fold does not yet
    // require it (#1178), and `account_is_fully_public` above inspects only streams this account
    // OWNS, so it says nothing about the foreign stream a grant points at. Contribution targets
    // `PublicRead` by construction, and `stream_access_mode` fails closed to `Private` when the
    // ownership fact has not been synced, so an unverifiable grant does not qualify either.
    //
    // The exposure is stated rather than implied: a qualifying contributor's account log becomes
    // readable by ANY dialer, since public admission is anonymous. Its authored content is on a
    // public stream by the check below; what this adds is the contributor's own roster metadata.
    for (repo_id, owner) in crate::memory_write::contribution_targets(conn)? {
        let stream = rag_rat_oplog::owner_stream_v2_id_for_account(
            &repo_id,
            owner,
            rag_rat_oplog::AccessMode::PublicRead,
        )?;
        if contribution_stream_is_servable(conn, owner, stream, account)? {
            return Ok(true);
        }
    }
    // Evidence of PAST authorship, not just current configuration (#1185): re-pointing `sync
    // contribute` at another owner must not strand contributions already authored onto the
    // previous owner's stream — that owner's pull would fall back to `Closed` against this store
    // even though its grant is still effective and the entries sit on its PublicRead stream. The
    // streams this account has actually authored accepted entries onto are durable facts the
    // config cannot erase; each is verified exactly like a configured target (ownership fact
    // synced, PublicRead, live writer grant), so a stale or revoked authorship exposes nothing.
    // The enumeration rides the V117 author-leading partial index — this still runs per dial,
    // before authentication.
    for stream in rag_rat_oplog::authored_foreign_streams(conn, account)? {
        let Some(owner) = rag_rat_oplog::stream_owner_account(conn, stream)? else {
            continue;
        };
        if contribution_stream_is_servable(conn, owner, stream, account)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// One contribution stream's servability check, identical for a CONFIGURED target and an
/// AUTHORED-evidence one: the owner's ownership fact must be synced and declare `PublicRead`
/// (`stream_access_mode` fails closed to `Private` when it is not), and the grant must still be
/// effective.
///
/// Shared with [`crate::memory_write`]'s private-stream guard, which must block on exactly the
/// authorship this predicate says is still reachable — otherwise it refuses a store whose
/// contributions the owner already cannot pull, permanently and with no recourse.
pub(crate) fn contribution_stream_is_servable(
    conn: &Connection,
    owner: rag_rat_oplog::AccountId,
    stream: rag_rat_oplog::StreamId,
    account: rag_rat_oplog::AccountId,
) -> anyhow::Result<bool> {
    if rag_rat_oplog::stream_access_mode(conn, owner, stream)?
        != rag_rat_oplog::AccessMode::PublicRead
    {
        return Ok(false);
    }
    Ok(rag_rat_oplog::effective_writer_grant(conn, owner, stream, account)?.is_some())
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
    // An over-size envelope is LATCHED to `None` in the record, not filtered on the way out.
    // Nothing `matches` compares moves when the ceiling or the wrap layout does, so the same
    // record answers this question on every one-second tick — a filter alone would repeat the
    // verdict forever. Latched, the roster is reported once and every later tick reads `None`.
    // This covers a fresh seal and a record written before the ceiling moved alike, so the two
    // never give an operator two different accounts of the same roster.
    if record.envelope.as_deref().is_some_and(|envelope| !fits_one_announcement(envelope)) {
        write_advertisement(conn, &PersistedAdvertisement { envelope: None, ..record })?;
        return Ok(None);
    }
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

/// Whether this envelope can be advertised at all, said in words an operator can act on.
///
/// The publish boundary refuses an over-size envelope too, but it sees bytes and not a roster, so
/// its message names a byte count and no action. This one names the roster and its ceiling, which
/// is the only thing an operator can change.
///
/// The verdict is warned, so a caller on a timer MUST latch it — see `prepare_advertisement`.
fn fits_one_announcement(envelope: &[u8]) -> bool {
    if rag_rat_sync::discovery::fits_publish(envelope) {
        return true;
    }
    tracing::warn!(
        // The envelope's size law: one version byte, then one fixed-size wrap per recipient.
        recipients = (envelope.len() - 1) / rag_rat_oplog::discovery::WRAP_LEN,
        bytes = envelope.len(),
        max_recipients = rag_rat_sync::discovery::MAX_PUBLISHABLE_RECIPIENTS,
        "not advertising: this account's roster is too large to seal into one announcement"
    );
    false
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
    // Size is NOT judged here. A fresh seal and a persisted envelope are the same input to the
    // question, and asking it once — see `fits_one_announcement` — is what keeps the two paths from
    // giving an operator two different accounts of the same roster.
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
    let (exchange, opener) = match discovery_fetch(config, conn, &relay)? {
        Some(fetch) => (
            Some(rag_rat_sync::discovery::DiscoveryExchange {
                endpoint,
                service: fetch.service,
                tag: fetch.tag,
                fetch: true,
                publish: None,
                ttl_seconds: rag_rat_sync::discovery::publish_ttl_seconds(
                    config.sync.push_interval_secs,
                ),
            }),
            fetch.opener,
        ),
        None => (None, None),
    };
    let resolved =
        rag_rat_sync::discover_peers(&config.sync.server_peers, &relay, exchange, &|payload| {
            opener.as_ref().and_then(|opener| opener.open(payload))
        })
        .await;
    // Scope the DEVICE-sync phases to peers that can serve this account. A peer memoized as a
    // FOREIGN-account host serves only its own account by construction, so dialing it here would
    // fail the account-scope handshake, log a device-sync failure, and count an error on every
    // cadence — while the cross-account pass below succeeds against the same host. Until the
    // first successful foreign pull memoizes a host it is still dialed once per pass; that
    // warmup noise is bounded and self-healing, unlike the permanent false alarm it replaces.
    let foreign_hosts = foreign_pull_hosts(conn)?;
    let device_peers: Vec<(String, rag_rat_sync::EndpointAddr)> = resolved
        .peers
        .into_iter()
        .filter(|(_, address)| !foreign_hosts.contains(&Ok(*address.id.as_bytes())))
        .collect();
    let mut reached = vec![false; device_peers.len()];
    for (index, (peer, address)) in device_peers.iter().enumerate() {
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
    for (index, (peer, address)) in device_peers.iter().enumerate() {
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
    for (index, (peer, address)) in device_peers.iter().enumerate() {
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
    // Cross-account contribution (#1175): pull each foreign account this store depends on, so
    // memories move on the same trigger as device sync — no command required. Failures are logged
    // and retried on the next cadence; they never fail the device-sync pass.
    if let Err(error) = pull_foreign_accounts(config, conn, endpoint, account).await {
        tracing::warn!(%error, "cross-account pull pass failed; the next cadence retries");
    }
    // Resolve any anchors this run's table reconciliation pulled against the local index, so they
    // surface as drive-by without waiting for the next index open (idempotent when nothing
    // changed).
    crate::resolve_synced_distill_anchors(conn)?;
    let ok = reached.iter().filter(|reached| **reached).count();
    let peers = reached.len() + resolved.unresolved_configured;
    Ok((peers, ok, peers - ok))
}

/// The foreign accounts automatic sync must pull. Content is offered by AUTHOR, so each direction
/// of contribution (#1164) needs the OTHER side's account synced here:
///
/// * each configured contribution owner — this store authors onto the owner's stream and needs the
///   owner's log for authority plus the owner's content for read-back;
/// * each effective writer grantee of this account — the grantee's contributions sit on THIS
///   account's streams but only a session scoped to the GRANTEE's account carries them;
/// * each subscribed owner (#1156) — a read-only mirror is pull-only, and without its account here
///   a subscription would be configured but never fetch anything.
fn foreign_pull_targets(
    conn: &Connection,
    local: rag_rat_oplog::AccountId,
) -> anyhow::Result<Vec<rag_rat_oplog::AccountId>> {
    let mut targets: Vec<rag_rat_oplog::AccountId> =
        crate::memory_write::contribution_targets(conn)?.into_iter().map(|(_, o)| o).collect();
    targets.extend(crate::memory_write::subscription_owners(conn)?);
    targets.extend(rag_rat_oplog::effective_writer_grantees(conn, local)?);
    targets.sort_unstable_by_key(|target| target.to_bytes());
    targets.dedup();
    targets.retain(|target| *target != local);
    Ok(targets)
}

/// Which peer answered for which foreign account, so quiet cycles dial ONE peer instead of
/// re-probing (and re-warning about) every configured peer that does not hold the account.
const PULL_PEER_MEMO_PREFIX: &str = "sync_pull_peer:";

/// The identity a peer STRING names, for comparing peers across spellings: the parsed 32-byte
/// node id when the string parses (hex and base32 spellings of one node compare equal), or the
/// trimmed literal otherwise (an unparseable entry still compares as itself). Node-id strings
/// must never be compared literally — `[sync] server_peers` accepts several spellings of one
/// node, and the memo stores whichever spelling answered.
fn peer_identity(peer: &str) -> Result<[u8; 32], String> {
    rag_rat_sync::parse_node_id(peer).map_err(|_| peer.trim().to_string())
}

/// Peers memoized as FOREIGN-account hosts, as [`peer_identity`] values. A production host serves
/// only its OWN account, so the local-account (device sync) phase can never succeed against one of
/// these — dialing them there fails the account-scope handshake and reads as a broken device sync
/// every cadence.
fn foreign_pull_hosts(conn: &Connection) -> anyhow::Result<Vec<Result<[u8; 32], String>>> {
    Ok(rag_rat_db::meta::meta_values_with_prefix(conn, PULL_PEER_MEMO_PREFIX)?
        .iter()
        .map(|peer| peer_identity(peer))
        .collect())
}

/// The peers to dial for one foreign account, memoized answerer first. The memo ORDERS the
/// configured peer set, never extends it: a host removed from `[sync] server_peers` must stop
/// being dialed, so a memo that is no longer configured is cleared rather than honored.
/// Membership is decided by [`peer_identity`], so a memo still counts as configured when the
/// config spells the same node differently.
fn ordered_pull_peers(
    conn: &Connection,
    memo_key: &str,
    peers: &[String],
) -> anyhow::Result<Vec<String>> {
    let memo = rag_rat_db::meta::read_meta(conn, memo_key)?
        .filter(|memo| peers.iter().any(|peer| peer_identity(peer) == peer_identity(memo)));
    if memo.is_none() {
        // Clears a decommissioned host's memo; a no-op when nothing was stored.
        rag_rat_db::meta::delete_meta(conn, memo_key)?;
    }
    let memo_identity = memo.as_deref().map(peer_identity);
    Ok(memo
        .iter()
        .chain(peers.iter().filter(|peer| Some(peer_identity(peer)) != memo_identity))
        .cloned()
        .collect())
}

async fn pull_foreign_accounts(
    config: &Config,
    conn: &Connection,
    endpoint: &iroh::Endpoint,
    local: rag_rat_oplog::AccountId,
) -> anyhow::Result<()> {
    let targets = foreign_pull_targets(conn, local)?;
    if targets.is_empty() {
        return Ok(());
    }
    let peers = &config.sync.server_peers;
    if peers.is_empty() {
        // Discovery cannot stand in: a foreign account's discovery tag derives from that
        // account's own secret, which only its own devices hold.
        tracing::warn!(
            "cross-account sync has accounts to pull but no [sync] server_peers to pull from"
        );
        return Ok(());
    }
    let relay = relay_url(config);
    for target in targets {
        let account_hex = hash::hex_lower(&target.to_bytes());
        let memo_key = format!("{PULL_PEER_MEMO_PREFIX}{account_hex}");
        let ordered: Vec<(String, rag_rat_sync::EndpointAddr)> = ordered_pull_peers(
            conn, &memo_key, peers,
        )?
        .into_iter()
        .filter_map(|peer| match rag_rat_sync::peer_addr(&peer, &relay) {
            Ok(addr) => Some((peer, addr)),
            Err(error) => {
                tracing::warn!(peer, %error, "skipping cross-account peer: invalid node id");
                None
            },
        })
        .collect();
        let outcome = pull_account_via_peers(conn, endpoint, target, &ordered).await?;
        match outcome.peer {
            Some(peer) => rag_rat_db::meta::set_meta(conn, &memo_key, &peer)?,
            None => tracing::warn!(
                account = %account_hex,
                error = outcome.last_error.as_deref().unwrap_or("no peer reachable"),
                "cross-account pull did not complete; the next cadence retries"
            ),
        }
    }
    // Materialize whatever landed, once for the whole pass (idempotent when nothing changed).
    crate::drain_synced_memory(conn)?;
    Ok(())
}

/// The outcome of pulling one FOREIGN account across a set of candidate peers.
pub struct ForeignPullOutcome {
    /// The peer that completed the pull: account log converged, capability sufficient, content
    /// converged. `None` when every peer failed a gate.
    pub peer: Option<String>,
    /// Entries stored across ALL attempts. Durable across a failed peer: a peer can store entries
    /// and then miss convergence, and those bytes stay — reporting only the final peer's tally
    /// would undercount, sometimes to zero.
    pub account_entries: usize,
    pub content_entries: usize,
    /// The most recent per-peer failure, for reporting when `peer` is `None`.
    pub last_error: Option<String>,
}

/// Pull a foreign account's log and content from the first of `peers` that can actually serve it.
/// This is the shared primitive behind `rag-rat sync pull` and the automatic cross-account pass —
/// the admission gates below are security decisions and must not drift between the two.
pub async fn pull_account_via_peers(
    conn: &Connection,
    endpoint: &iroh::Endpoint,
    target: rag_rat_oplog::AccountId,
    peers: &[(String, rag_rat_sync::EndpointAddr)],
) -> anyhow::Result<ForeignPullOutcome> {
    let mut outcome =
        ForeignPullOutcome { peer: None, account_entries: 0, content_entries: 0, last_error: None };
    for (peer_id, addr) in peers {
        // ACCOUNT LOG FIRST, then content: content acceptance re-derives authority from the
        // account log, so a content session run first would park every candidate until a later
        // settle. One pass, correct order, nothing parked in the normal case.
        //
        // `PublicRead`, never `Closed`: on first contact this store holds ZERO roster facts for
        // the foreign account, so `authorize` returns `Unavailable` — which `Closed` maps to
        // `Unauthorized`, failing every first pull. `PublicRead` maps `Unavailable` + dialer to
        // the ReadWrite bootstrap fallback built for exactly this. Admission is not trust:
        // `account_ingest` / `content_ingest` re-verify every entry from scratch.
        let mut account_store = OplogSyncStore::new(conn, target, time::now_ms);
        let account_report = match rag_rat_sync::connect_and_reconcile(
            endpoint,
            addr.clone(),
            rag_rat_sync::SYNC_ALPN,
            &mut account_store,
            AuthPolicy::PublicRead,
            time::now_ms,
            rag_rat_sync::MAX_RECONCILE_ROUNDS,
        )
        .await
        {
            Ok(report) => report,
            Err(error) => {
                outcome.last_error = Some(format!("{peer_id}: account log: {error}"));
                continue;
            },
        };
        outcome.account_entries += account_report.entries_newly_stored;
        // A pull exists to RECEIVE. If this side granted the peer only `ReadOnly`, its entries
        // are rejected on arrival, so an all-quiet round means "structurally unable to receive"
        // rather than "in sync" — and `converged` would report success on an incomplete
        // account. This is the resumed-bootstrap wedge: once a partial pull leaves
        // `account_effective_count > 0` for the target, a serving device whose `DeviceAdd` has
        // not arrived folds `Rejected` (not `Unavailable`), which loses the bootstrap fallback.
        if account_report.peer_capability != PeerCapability::ReadWrite {
            outcome.last_error = Some(format!(
                "{peer_id}: this store holds a PARTIAL roster for that account, so it could not \
                 authorize this peer to serve — the peer was admitted read-only and sent nothing. \
                 Pull from the peer whose device is already in the roster you hold (usually the \
                 account's own host), or start from a store with no entries for it"
            ));
            continue;
        }
        // A quiet round can also mean the peer simply had nothing: an EMPTY account store
        // completes the PublicRead protocol, and `Unavailable` hands the dialer the bootstrap
        // ReadWrite capability, so round one is quiet and `converged` is true without this
        // store ever learning the account. Require the target to actually be known here.
        if rag_rat_oplog::account_effective_count(conn, target)? == 0 {
            outcome.last_error = Some(format!(
                "{peer_id}: completed the exchange without sending account {}'s log — it does not \
                 hold that account. Check the id, or point at a machine that does",
                hash::hex_lower(&target.to_bytes())
            ));
            continue;
        }
        // A non-converged account leg means the round cap was hit with the store still possibly
        // incomplete. Content acceptance re-derives authority from that log, so proceeding would
        // silently leave valid entries unaccepted. Treat the peer as unusable and try the next.
        if !account_report.converged {
            outcome.last_error = Some(format!(
                "{peer_id}: the account log did not converge before the round limit; its content \
                 would be judged against incomplete authority"
            ));
            continue;
        }
        let mut content_store = OplogContentSyncStore::new(conn, target, time::now_ms);
        let content_report = match rag_rat_sync::connect_and_reconcile(
            endpoint,
            addr.clone(),
            rag_rat_sync::CONTENT_SYNC_ALPN,
            &mut content_store,
            AuthPolicy::PublicRead,
            time::now_ms,
            rag_rat_sync::MAX_RECONCILE_ROUNDS,
        )
        .await
        {
            Ok(report) => report,
            Err(error) => {
                outcome.last_error = Some(format!("{peer_id}: content: {error}"));
                continue;
            },
        };
        outcome.content_entries += content_report.entries_newly_stored;
        if !content_report.converged {
            // Same treatment as the account leg: a healthy later peer may finish the job, and
            // breaking here would pin every re-run on the same non-converging first peer. The
            // entries this peer did store are durable and stay counted.
            outcome.last_error =
                Some(format!("{peer_id}: content did not converge before the round limit"));
            continue;
        }
        outcome.peer = Some(peer_id.clone());
        break;
    }
    Ok(outcome)
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

/// What one discovery fetch needs: where to ask, under which tag, and what opens the answers.
struct DiscoveryFetch {
    tag: [u8; 32],
    service: rag_rat_sync::EndpointAddr,
    /// `None` when this store cannot open announcements at all — no account, no local device, or a
    /// failed read. Discovery then finds nothing and the pass falls back to the configured peers.
    opener: Option<rag_rat_oplog::discovery::AnnouncementOpener>,
}

/// Resolve that state once, before the fetch, or `None` when there will be no fetch: discovery is
/// off, or the configured service node id is unusable, or this store has no discovery secret.
///
/// The opener is loaded here rather than in the per-payload closure because the account and this
/// device's key are the same for every announcement in a pass. **The service gate comes before
/// every read** so a pass that will not fetch pays for none of them: the opener costs an account
/// read plus a device load that re-derives and validates the stored keys, and with no exchange to
/// pass on, `discover_peers` returns before it ever calls the opening closure.
fn discovery_fetch(
    config: &Config,
    conn: &Connection,
    relay: &str,
) -> anyhow::Result<Option<DiscoveryFetch>> {
    let Some(service) = discovery_addr(config, relay) else {
        return Ok(None);
    };
    let Some(secret) = rag_rat_sync::discovery::discovery_secret(conn)? else {
        return Ok(None);
    };
    let tag = rag_rat_sync::discovery::account_tag(&secret);
    // A failed load leaves discovery unopenable and falls back to the configured peers, the same as
    // a payload that will not open — the rest of the pass has work to do and should not fail with
    // it. It is wider than the per-payload failure it stands in for, though: one bad read costs the
    // whole pass rather than one announcement, so a persistent one would otherwise look like
    // discovery quietly finding nobody. Log it.
    let opener = rag_rat_oplog::discovery::AnnouncementOpener::load(conn, &tag)
        .inspect_err(|error| {
            tracing::warn!(%error, "discovery announcements are unopenable this pass");
        })
        .ok()
        .flatten();
    Ok(Some(DiscoveryFetch { tag, service, opener }))
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
    for (index, pair) in encoded.as_bytes().as_chunks::<2>().0.iter().enumerate() {
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
    use rag_rat_base::{hash, time};
    use rag_rat_sync::AuthPolicy;
    use rusqlite::Connection;

    use super::{
        DISCOVERY_ADVERTISEMENT, DeviceSyncOutcome, PULL_PEER_MEMO_PREFIX, PerPeerSessionLimiter,
        PersistedAdvertisement, RESIDENT_NUDGE, RefusedPublication, account_is_public_kb, can_host,
        can_sync, device_sync_run, discovery_fetch, foreign_pull_hosts, foreign_pull_targets,
        nudge_resident_host, ordered_pull_peers, peer_identity, prepare_advertisement,
        pull_account_via_peers, read_advertisement, refused_publication_is_due, retry_is_due,
        write_advertisement,
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
    /// syncing the CONTRIBUTOR's account.
    ///
    /// The evidence is deliberately narrow: a live grant on a stream this store is CONFIGURED to
    /// contribute to. A grant alone is not enough — a stale one this store never uses must not
    /// expose it — and the narrow question is also the cheap one, since it resolves to indexed
    /// point lookups instead of an unindexed grantee scan on every pre-auth connection.
    #[test]
    fn only_a_configured_contribution_grant_makes_a_stream_less_account_servable() {
        use rusqlite::{Transaction, TransactionBehavior};

        // A real owner with a real PublicRead stream for `repo-a`, granting the contributor.
        let owner = schema_conn();
        let owner_account = rag_rat_oplog::local_account(&owner, 1_000).unwrap();
        let public_stream = {
            let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
            let s = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
                &tx,
                "repo-a",
                rag_rat_oplog::AccessMode::PublicRead,
                1_000,
            )
            .unwrap();
            tx.commit().unwrap();
            s
        };

        let contributor = schema_conn();
        let account = rag_rat_oplog::local_account(&contributor, 1_000).unwrap();
        contributor
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
                 ('repo-a','a',0)",
                [],
            )
            .unwrap();
        // Owns nothing and contributes nowhere: refused, as a vacuously-public account must be.
        assert!(!account_is_public_kb(&contributor, account).unwrap());

        {
            let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
            rag_rat_oplog::author_stream_grant_in_tx(
                &tx,
                public_stream,
                account,
                rag_rat_oplog::GrantRole::Writer,
                1_000,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        for entry in rag_rat_oplog::account_entries_for_sync(&owner, owner_account).unwrap() {
            rag_rat_oplog::account_ingest(&contributor, &entry.signed_bytes, 1_000).unwrap();
        }

        // The grant is held and verifiable — but this store does not contribute anywhere, so it is
        // still not servable. A grant it never uses is not a reason to expose it.
        assert!(
            !account_is_public_kb(&contributor, account).unwrap(),
            "an unused grant does not expose the account",
        );

        // Configure the contribution, and NOW it is servable.
        rag_rat_db::meta::set_repo_meta(
            &contributor,
            "repo-a",
            "memory_contribution_owner",
            &rag_rat_base::hash::hex_lower(&owner_account.to_bytes()),
        )
        .unwrap();
        assert!(
            account_is_public_kb(&contributor, account).unwrap(),
            "a live grant on the stream this store contributes to makes it servable",
        );

        // Revoking closes the door again.
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

    /// A pass that will not fetch reads nothing to fetch with. The opener costs an account read
    /// plus a device load that re-derives and validates this device's keys, and with no
    /// discovery service `discover_peers` returns before it ever calls the opening closure.
    ///
    /// Pinned by poisoning the reads so any of them fails loudly: with discovery off the call still
    /// succeeds, and turning discovery back on surfaces the failure, so the silence is the gate and
    /// not a store with nothing to read.
    #[test]
    fn discovery_off_reads_nothing_the_pass_cannot_use() {
        let poisoned = schema_conn();
        poisoned.execute("DROP TABLE oplog_local_account", []).unwrap();
        let mut config = Config::minimal_for_database(
            PathBuf::from("/nonexistent/sync.sqlite"),
            PathBuf::from("/nonexistent"),
        );

        config.sync.discovery = false;
        assert!(
            discovery_fetch(&config, &poisoned, "https://relay.one").unwrap().is_none(),
            "no service to fetch from, so nothing is read to open with"
        );

        config.sync.discovery = true;
        assert!(
            discovery_fetch(&config, &poisoned, "https://relay.one").is_err(),
            "the reads do happen once there is a fetch to open for"
        );

        // A healthy store loads exactly one opener, carried for the whole pass.
        let conn = schema_conn();
        rag_rat_oplog::local_account(&conn, 1_000).unwrap();
        let fetch = discovery_fetch(&config, &conn, "https://relay.one")
            .unwrap()
            .expect("an account plus a valid service node id is a fetch");
        assert!(
            fetch.opener.is_some(),
            "a founder is a device that can open its own account's tag"
        );
    }

    #[test]
    fn only_writers_can_host_while_read_only_devices_can_dial() {
        assert!(can_host(Some(rag_rat_sync::PeerCapability::ReadWrite)));
        assert!(!can_host(Some(rag_rat_sync::PeerCapability::ReadOnly)));
        assert!(can_sync(Some(rag_rat_sync::PeerCapability::ReadOnly)));
    }

    const ADVERTISED_NODE: [u8; 32] = [0x11; 32];
    const ADVERTISED_SERVICE: [u8; 32] = [0x22; 32];
    const ADVERTISED_RELAY: &str = "https://relay.one";

    /// Persist an advertisement record `prepare_advertisement` matches, so it is reused verbatim
    /// rather than resealed — the shape in which an envelope sealed under an older byte ceiling or
    /// wrap layout survives.
    fn persist_advertised_envelope(database: &std::path::Path, envelope: Vec<u8>) {
        let storage = super::IndexConnection::open(database).unwrap();
        let conn = storage.connection();
        rag_rat_db::schema::apply(conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        rag_rat_oplog::local_account(conn, 1_000).unwrap();
        let secret = rag_rat_sync::discovery::discovery_secret(conn).unwrap().unwrap();
        write_advertisement(conn, &PersistedAdvertisement {
            tag: rag_rat_sync::discovery::account_tag(&secret),
            node: ADVERTISED_NODE,
            service: ADVERTISED_SERVICE,
            relay: ADVERTISED_RELAY.to_owned(),
            roster_stamp: rag_rat_oplog::discovery::roster_stamp(conn).unwrap(),
            envelope: Some(envelope),
            published_at_ms: None,
            ttl_seconds: 600,
        })
        .unwrap();
    }

    /// One advertisement pass, with whatever it warned about.
    ///
    /// Every pass goes through the capturing subscriber, and that is not decoration: `tracing`
    /// caches a callsite's interest PROCESS-wide on its first use, so a single pass made with no
    /// subscriber installed caches "never" for the size warning and every later capture in the same
    /// test binary comes back empty. Routing all of them through one seam takes that out of the
    /// hands of test ordering.
    fn prepare_advertised(database: &std::path::Path) -> (Option<super::Publication>, String) {
        let mut prepared = None;
        let logged = captured_warnings(|| {
            prepared = prepare_advertisement(
                database,
                &ADVERTISED_NODE,
                &ADVERTISED_SERVICE,
                ADVERTISED_RELAY,
                2_000,
                600,
            )
            .unwrap();
        });
        (prepared, logged)
    }

    /// A `MakeWriter` that appends every formatted log line into a shared buffer, so a test can
    /// assert on the `tracing` events a pass emitted — and on how many times.
    #[derive(Clone)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `body` with warnings captured, returning what it logged. The subscriber is thread-local
    /// (`with_default`), so parallel tests do not see each other's output.
    fn captured_warnings(body: impl FnOnce()) -> String {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(CaptureWriter(std::sync::Arc::clone(&buffer)))
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let logged = buffer.lock().unwrap().clone();
        String::from_utf8(logged).expect("formatted log lines are UTF-8")
    }

    /// A persisted envelope is judged against the byte ceiling too, not just a fresh seal.
    ///
    /// The record is reused verbatim whenever the tag, endpoint, service, relay, and roster stamp
    /// all still match, and none of those move when the ceiling or the wrap layout does — so an
    /// envelope sealed under the old law would sail past the seal-time verdict, cost a dial, and be
    /// reported at the publish boundary as a byte count with no roster in it. The under-size half
    /// is what keeps this from passing for the wrong reason: the same record one byte smaller must
    /// still be advertised.
    #[test]
    fn a_persisted_envelope_over_the_announcement_ceiling_is_not_advertised() {
        let dir = tempfile::TempDir::new().unwrap();
        let database = dir.path().join("index.sqlite");
        let ceiling = rag_rat_sync::discovery::MAX_ANNOUNCEMENT_BYTES;

        persist_advertised_envelope(&database, vec![0; ceiling + 1]);
        assert!(
            prepare_advertised(&database).0.is_none(),
            "an over-size persisted envelope must not be handed to the publish path"
        );

        persist_advertised_envelope(&database, vec![0; ceiling]);
        assert!(
            prepare_advertised(&database).0.is_some(),
            "the ceiling itself fits, so the same record one byte smaller is still advertised"
        );
    }

    /// The over-size verdict is reported ONCE, however long the roster stays too large.
    ///
    /// `prepare_advertisement` runs on the one-second `ADVERTISEMENT_REFRESH` tick, and nothing the
    /// record `matches` on moves when the ceiling does — so a verdict re-derived on every pass and
    /// not latched is a warning per second, indefinitely, burying every other line the operator
    /// needs. Latching the record to `envelope: None` is what holds it to one, and it is also what
    /// keeps the dial suppressed: the publish rate limiter never sees this condition, because the
    /// pass returns before `exchange`.
    #[test]
    fn an_over_size_roster_is_reported_once_and_not_on_every_pass() {
        let dir = tempfile::TempDir::new().unwrap();
        let database = dir.path().join("index.sqlite");
        let ceiling = rag_rat_sync::discovery::MAX_ANNOUNCEMENT_BYTES;
        persist_advertised_envelope(&database, vec![0; ceiling + 1]);

        let reported: usize = (0..10)
            .map(|_| {
                let (publication, logged) = prepare_advertised(&database);
                assert!(publication.is_none(), "an over-size roster is never advertised");
                logged.matches("roster is too large to seal into one announcement").count()
            })
            .sum();
        assert_eq!(reported, 1, "ten passes must report the over-size roster once, not once each");

        let storage = super::IndexConnection::open(&database).unwrap();
        assert_eq!(
            read_advertisement(storage.connection()).unwrap().unwrap().envelope,
            None,
            "the record is latched, so there is no envelope left for a later pass to judge"
        );
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

    #[test]
    fn foreign_pull_targets_cover_both_directions_and_exclude_the_local_account() {
        use rusqlite::{Transaction, TransactionBehavior};

        let store = schema_conn();
        let local = rag_rat_oplog::local_account(&store, 1_000).unwrap();
        assert!(foreign_pull_targets(&store, local).unwrap().is_empty());

        // Contributor direction: a configured contribution owner becomes a pull target.
        store
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
                 ('repo-a','a',0)",
                [],
            )
            .unwrap();
        let owner = rag_rat_oplog::AccountId::from_bytes([0x77; 32]);
        rag_rat_db::meta::set_repo_meta(
            &store,
            "repo-a",
            "memory_contribution_owner",
            &hash::hex_lower(&owner.to_bytes()),
        )
        .unwrap();
        assert_eq!(foreign_pull_targets(&store, local).unwrap(), vec![owner]);

        // Owner direction: an effective writer grantee of THIS account becomes a pull target too.
        let grantee = rag_rat_oplog::AccountId::from_bytes([0x22; 32]);
        {
            let tx = Transaction::new_unchecked(&store, TransactionBehavior::Immediate).unwrap();
            let stream = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
                &tx,
                "repo-a",
                rag_rat_oplog::AccessMode::PublicRead,
                1_000,
            )
            .unwrap();
            rag_rat_oplog::author_stream_grant_in_tx(
                &tx,
                stream,
                grantee,
                rag_rat_oplog::GrantRole::Writer,
                1_000,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let targets = foreign_pull_targets(&store, local).unwrap();
        assert!(targets.contains(&owner) && targets.contains(&grantee));
        assert_eq!(targets.len(), 2);

        // A repo configured (nonsensically) to contribute to the LOCAL account never self-pulls.
        store
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
                 ('repo-b','b',0)",
                [],
            )
            .unwrap();
        rag_rat_db::meta::set_repo_meta(
            &store,
            "repo-b",
            "memory_contribution_owner",
            &hash::hex_lower(&local.to_bytes()),
        )
        .unwrap();
        assert_eq!(foreign_pull_targets(&store, local).unwrap().len(), 2);
    }

    /// A read-only SUBSCRIBER (#1156) is pull-only: it authors nothing and is never served, so
    /// nothing but this enumeration would ever fetch the owner's account — a subscription missing
    /// here is configured but permanently empty.
    #[test]
    fn a_subscribed_owner_is_a_foreign_pull_target() {
        let store = schema_conn();
        let local = rag_rat_oplog::local_account(&store, 1_000).unwrap();
        store
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
                 ('repo-a','a',0)",
                [],
            )
            .unwrap();
        let owner = rag_rat_oplog::AccountId::from_bytes([0x55; 32]);
        rag_rat_db::meta::set_repo_meta(
            &store,
            "repo-a",
            "memory_subscription_owner",
            &hash::hex_lower(&owner.to_bytes()),
        )
        .unwrap();
        assert_eq!(foreign_pull_targets(&store, local).unwrap(), vec![owner]);
    }

    #[test]
    fn a_memoized_peer_orders_the_configured_set_and_a_decommissioned_one_is_cleared() {
        let conn = schema_conn();
        let key = format!("{PULL_PEER_MEMO_PREFIX}aa");
        let peers = vec!["node-a".to_string(), "node-b".to_string()];

        // No memo: configured order, nothing stored.
        assert_eq!(ordered_pull_peers(&conn, &key, &peers).unwrap(), peers);

        // A memoized answerer moves to the front without duplicating.
        rag_rat_db::meta::set_meta(&conn, &key, "node-b").unwrap();
        assert_eq!(ordered_pull_peers(&conn, &key, &peers).unwrap(), vec![
            "node-b".to_string(),
            "node-a".to_string()
        ]);

        // Decommissioned: the memoized host left [sync] server_peers, so it is neither dialed
        // nor kept — the memo orders the configured set, never extends it.
        let remaining = vec!["node-a".to_string()];
        assert_eq!(ordered_pull_peers(&conn, &key, &remaining).unwrap(), remaining);
        assert_eq!(rag_rat_db::meta::read_meta(&conn, &key).unwrap(), None, "stale memo cleared");
    }

    #[test]
    fn only_pull_memo_keys_mark_a_peer_as_a_foreign_host() {
        let conn = schema_conn();
        assert!(foreign_pull_hosts(&conn).unwrap().is_empty());
        rag_rat_db::meta::set_meta(&conn, &format!("{PULL_PEER_MEMO_PREFIX}aa"), "node-f").unwrap();
        // Unrelated meta keys — including other sync keys — never mark a device-sync peer.
        rag_rat_db::meta::set_meta(&conn, RESIDENT_NUDGE, "5").unwrap();
        rag_rat_db::meta::set_meta(&conn, "sync_pull_peerless", "node-x").unwrap();
        assert_eq!(foreign_pull_hosts(&conn).unwrap(), vec![peer_identity("node-f")]);
    }

    /// Standard base32, no padding — an alternate spelling `parse_node_id` accepts for the node a
    /// 64-char lowercase hex string names.
    fn base32_nopad(bytes: &[u8; 32]) -> String {
        const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut out = String::with_capacity(52);
        let (mut acc, mut bits) = (0u32, 0u32);
        for &byte in bytes {
            acc = (acc << 8) | u32::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPHABET[((acc >> bits) & 0x1f) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize] as char);
        }
        out
    }

    #[test]
    fn peer_comparisons_recognize_alternate_spellings_of_one_node() {
        let conn = schema_conn();
        let bytes = rag_rat_sync::node_id_from_secret([7; 32]);
        let hex = rag_rat_sync::node_id_to_string(&bytes).unwrap();
        let base32 = base32_nopad(&bytes);
        assert_ne!(hex, base32);

        // The memo holds the spelling that answered while the config spells the same node
        // differently: the memo still counts as configured (kept and fronted), and the
        // equivalent configured spelling is not dialed a second time.
        let key = format!("{PULL_PEER_MEMO_PREFIX}bb");
        rag_rat_db::meta::set_meta(&conn, &key, &base32).unwrap();
        let peers = vec![hex.clone(), "node-a".to_string()];
        assert_eq!(ordered_pull_peers(&conn, &key, &peers).unwrap(), vec![
            base32.clone(),
            "node-a".to_string()
        ]);
        assert!(rag_rat_db::meta::read_meta(&conn, &key).unwrap().is_some(), "memo kept");

        // Device sync excludes a memoized foreign host by parsed identity — the bytes a
        // resolved `EndpointAddr` carries — never by spelling.
        assert!(foreign_pull_hosts(&conn).unwrap().contains(&Ok(bytes)));
        assert_eq!(peer_identity(&hex), Ok(bytes));
    }

    /// A relay-free endpoint pair for exercising the pull helper over a real wire.
    async fn loopback_endpoints() -> (iroh::Endpoint, iroh::Endpoint) {
        let bind = |seed: [u8; 32]| async move {
            iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
                .alpns(vec![
                    rag_rat_sync::SYNC_ALPN.to_vec(),
                    rag_rat_sync::CONTENT_SYNC_ALPN.to_vec(),
                ])
                .relay_mode(iroh::RelayMode::Disabled)
                .secret_key(iroh::SecretKey::from_bytes(&seed))
                .bind()
                .await
                .unwrap()
        };
        (bind([0x31; 32]).await, bind([0x32; 32]).await)
    }

    /// A directly dialable address for a loopback endpoint (its 127.0.0.1 socket).
    fn direct_addr(endpoint: &iroh::Endpoint) -> iroh::EndpointAddr {
        let port = endpoint
            .addr()
            .ip_addrs()
            .next()
            .expect("a bound endpoint advertises at least one socket address")
            .port();
        iroh::EndpointAddr::new(endpoint.id())
            .with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
    }

    #[tokio::test]
    async fn pulling_a_contributors_account_lands_its_memory_in_the_owners_repo() {
        use rusqlite::{Transaction, TransactionBehavior};
        const NOW: i64 = 1_000;

        // OWNER: real account, PublicRead stream for `repo-a`, repo registered so the drain
        // mirrors the stream into `repo_memories`.
        let owner = schema_conn();
        let owner_account = rag_rat_oplog::local_account(&owner, NOW).unwrap();
        let stream = {
            let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
            let stream = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
                &tx,
                "repo-a",
                rag_rat_oplog::AccessMode::PublicRead,
                NOW,
            )
            .unwrap();
            tx.commit().unwrap();
            stream
        };
        owner
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
                 ('repo-a','a',0)",
                [],
            )
            .unwrap();

        // CONTRIBUTOR: separate identity, granted Writer, learns the grant from the owner's log.
        let contributor = schema_conn();
        let contributor_account = rag_rat_oplog::local_account(&contributor, NOW).unwrap();
        {
            let tx = Transaction::new_unchecked(&owner, TransactionBehavior::Immediate).unwrap();
            rag_rat_oplog::author_stream_grant_in_tx(
                &tx,
                stream,
                contributor_account,
                rag_rat_oplog::GrantRole::Writer,
                NOW,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        for entry in rag_rat_oplog::account_entries_for_sync(&owner, owner_account).unwrap() {
            rag_rat_oplog::account_ingest(&contributor, &entry.signed_bytes, NOW).unwrap();
        }
        let grant_id = rag_rat_oplog::effective_writer_grant(
            &contributor,
            owner_account,
            stream,
            contributor_account,
        )
        .unwrap()
        .expect("the grant reached the contributor");
        {
            let tx =
                Transaction::new_unchecked(&contributor, TransactionBehavior::Immediate).unwrap();
            rag_rat_oplog::author_grantee_content_batch_in_tx(
                &tx,
                stream,
                owner_account,
                grant_id,
                &[rag_rat_oplog::MemoryOp::NodeCreate {
                    node_id: rag_rat_oplog::NodeId::from("contributed-1"),
                    content: rag_rat_oplog::NodeContent {
                        kind: "Invariant".into(),
                        title: "from the contributor".into(),
                        body: "body".into(),
                        confidence: "high".into(),
                        source: "agent".into(),
                        tags: Vec::new(),
                        payload: None,
                    },
                }],
                NOW,
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // THE AUTOMATIC DIRECTION: the owner pulls the CONTRIBUTOR's account over a real wire
        // through the shared helper the reconcile pass and `sync pull` both use.
        let (contributor_ep, owner_ep) = loopback_endpoints().await;
        let peers = vec![("contributor-host".to_string(), direct_addr(&contributor_ep))];
        // Serve inbound connections until the pull finishes — a fixed accept count would hang
        // the test forever if the pull legitimately stopped after fewer connections.
        // The serving stores use REAL time: the dialing helper verifies the acceptor's node
        // binding against the wall clock, so a fixture clock would read as an expired binding.
        let server = async {
            loop {
                let mut serve_account = rag_rat_sync::OplogSyncStore::new(
                    &contributor,
                    contributor_account,
                    time::now_ms,
                );
                let mut serve_content = rag_rat_sync::OplogContentSyncStore::new(
                    &contributor,
                    contributor_account,
                    time::now_ms,
                );
                rag_rat_sync::accept_and_dispatch(
                    &contributor_ep,
                    &mut serve_account,
                    &mut serve_content,
                    AuthPolicy::PublicRead,
                    time::now_ms,
                )
                .await
                .unwrap();
            }
        };
        let pull = pull_account_via_peers(&owner, &owner_ep, contributor_account, &peers);
        let outcome = tokio::select! {
            outcome = pull => outcome.unwrap(),
            _ = server => unreachable!("the serve loop never exits"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                panic!("the pull did not finish within the test deadline")
            },
        };
        assert_eq!(outcome.peer.as_deref(), Some("contributor-host"), "{:?}", outcome.last_error);
        assert!(outcome.account_entries > 0, "the contributor's log arrived");
        assert!(outcome.content_entries > 0, "the contribution arrived");

        // The drain materializes the contribution into the owner's repo memories.
        rag_rat_oplog::settle_pending_content_refolds(
            &owner,
            &rag_rat_oplog::ContentRefoldBudget::unbounded(),
            NOW,
        )
        .unwrap();
        let effects = crate::drain_synced_memory(&owner).unwrap();
        assert!(effects.nodes_written >= 1, "the memory materialized: {effects:?}");
        let title: String = owner
            .query_row("SELECT title FROM repo_memories WHERE repo_id = 'repo-a'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "from the contributor");
    }
}
