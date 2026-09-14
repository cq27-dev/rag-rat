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
        let task =
            std::thread::Builder::new().name("rag-rat-sync".to_string()).spawn(move || {
                resident_worker(config, account, secret, relay, shutdown_rx, ready_tx)
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

/// The resident host's worker thread: it holds the sync session lock, owns the endpoint and runs
/// [`resident_loop`] until shutdown, reports startup through `ready`, and clears its heartbeat row
/// on the way out.
fn resident_worker(
    config: Config,
    account: rag_rat_oplog::AccountId,
    secret: Zeroizing<[u8; 32]>,
    relay: String,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    ready: std::sync::mpsc::SyncSender<anyhow::Result<ResidentHostReady>>,
) {
    let database = config.database.clone();
    // `WriteLock` records reentrancy per thread, so acquire and release it only on the worker that
    // owns the endpoint. It stays held through the final network wait.
    let session = match locks::WriteLock::acquire_sync_session_timeout(&database, LOCK_TIMEOUT) {
        Ok(Some(session)) => session,
        Ok(None) => {
            let _ = ready.send(Ok(ResidentHostReady::Unavailable));
            return;
        },
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        },
    };
    let _session = session;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
    let ready_for_runtime = ready.clone();
    let loop_database = database.clone();
    let result: anyhow::Result<()> = match runtime {
        Ok(runtime) => runtime.block_on(async move {
            let endpoint = rag_rat_sync::build_endpoint(*secret, &relay)
                .await
                .with_context(|| format!("binding the sync endpoint over relay {relay}"))?;
            let storage = IndexConnection::open(&loop_database)?;
            heartbeat(storage.connection())?;
            drop(storage);
            ready_for_runtime
                .send(Ok(ResidentHostReady::Started))
                .map_err(|_| anyhow!("resident sync host startup was abandoned"))?;
            tokio::task::LocalSet::new()
                .run_until(resident_loop(config, endpoint, account, loop_database, shutdown))
                .await;
            Ok(())
        }),
        Err(error) => Err(error.into()),
    };
    match IndexConnection::open(&database) {
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
        let _ = ready.send(Err(error));
    }
}

/// Record a durable hook request and report whether a resident host has recently heartbeated.
pub fn nudge_resident_host(conn: &Connection) -> anyhow::Result<bool> {
    let now = time::now_ms();
    rag_rat_db::meta::set_meta_i64(conn, RESIDENT_NUDGE, now)?;
    let heartbeat = rag_rat_db::meta::read_meta_i64(conn, RESIDENT_HEARTBEAT)?;
    // Unlike `within_window`, a heartbeat stamped ahead of `now` counts as recent here, and an age
    // of exactly `HEARTBEAT_MAX_AGE_MS` still does.
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
    outcome.map(|pass| DeviceSyncOutcome::Ran {
        peers: pass.peers,
        ok: pass.converged,
        errors: pass.peers - pass.converged,
    })
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
            let nudge = rag_rat_db::meta::read_meta_i64(conn, RESIDENT_NUDGE)?.unwrap_or_default();
            let now = time::now_ms();
            // `last_heartbeat == 0` means this process has not heartbeated yet; it is a flag, not a
            // timestamp to compare.
            if last_heartbeat == 0 || !within_window(last_heartbeat, now, HEARTBEAT_INTERVAL_MS) {
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

/// Who an announcement is sealed for and where it is published: the account tag, this endpoint,
/// the discovery service and the relay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AdvertisementIdentity {
    tag: [u8; 32],
    node: [u8; 32],
    service: [u8; 32],
    relay: String,
}

/// Persisted local state for one serving endpoint's announcement. The service appends rather than
/// replaces, so the exact sealed bytes and their possible liveness must survive a restart.
/// `identity` is flattened: the record is stored as JSON, and nesting it would change the stored
/// keys and strand every host's existing record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedAdvertisement {
    #[serde(flatten)]
    identity: AdvertisementIdentity,
    roster_stamp: Option<[u8; 32]>,
    envelope: Option<Vec<u8>>,
    published_at_ms: Option<i64>,
    ttl_seconds: u32,
}

impl PersistedAdvertisement {
    fn matches(&self, identity: &AdvertisementIdentity, stamp: Option<&[u8; 32]>) -> bool {
        self.identity == *identity && self.roster_stamp.as_ref() == stamp
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
    identity: AdvertisementIdentity,
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
            AdvertisementEndpoint { node: &node, service: &service_node, relay: &relay },
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
                tag: publication.identity.tag,
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

struct AdvertisementEndpoint<'a> {
    node: &'a [u8; 32],
    service: &'a [u8; 32],
    relay: &'a str,
}

fn prepare_advertisement(
    database: &Path,
    endpoint: AdvertisementEndpoint<'_>,
    now_ms: i64,
    ttl_seconds: u32,
) -> anyhow::Result<Option<Publication>> {
    let storage = IndexConnection::open(database)?;
    let conn = storage.connection();
    let Some(secret) = rag_rat_sync::discovery::discovery_secret(conn)? else {
        return Ok(None);
    };
    let identity = AdvertisementIdentity {
        tag: rag_rat_sync::discovery::account_tag(&secret),
        node: *endpoint.node,
        service: *endpoint.service,
        relay: endpoint.relay.to_owned(),
    };
    // The stamp includes sealing policy, so upgrading padding also replaces cached envelopes.
    let stamp = rag_rat_oplog::discovery::roster_stamp(conn)?;
    let persisted = read_advertisement(conn)?;
    let current = persisted.filter(|record| record.matches(&identity, stamp.as_ref()));
    let record = match current {
        Some(record) => record,
        None => {
            let envelope = seal_advertisement(conn, &identity)?;
            let record = PersistedAdvertisement {
                identity: identity.clone(),
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
    Ok(record.envelope.map(|envelope| Publication { identity, envelope, ttl_seconds }))
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
        // Above the padding floor, every wrap represents an actual recipient.
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
    if !record.matches(&publication.identity, stamp.as_ref())
        || record.envelope.as_deref() != Some(publication.envelope.as_slice())
    {
        return Ok(());
    }
    record.published_at_ms = Some(attempted_at_ms);
    record.ttl_seconds = publication.ttl_seconds;
    write_advertisement(conn, &record)
}

fn seal_advertisement(
    conn: &Connection,
    identity: &AdvertisementIdentity,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(sealed) =
        rag_rat_oplog::discovery::seal_discovery_announcement(conn, &identity.tag, &identity.node)?
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
    Ok(rag_rat_db::meta::set_meta(conn, DISCOVERY_ADVERTISEMENT, &serde_json::to_string(record)?)?)
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
                let (stream, report) = rag_rat_sync::dispatch_connection(
                    connection,
                    node,
                    &mut account_store,
                    &mut content_store,
                    policy,
                    time::now_ms,
                    Some(egress),
                )
                .await?;
                match stream {
                    rag_rat_sync::SyncAlpn::Content => {
                        crate::drain_synced_memory(conn)?;
                    },
                    // The resident host holds the index open, so the on-open re-resolution never
                    // re-fires for anchors pushed here — resolve them at the session's settle
                    // point.
                    rag_rat_sync::SyncAlpn::Table => {
                        crate::resolve_synced_distill_anchors(conn)?;
                    },
                    rag_rat_sync::SyncAlpn::Account | rag_rat_sync::SyncAlpn::Enroll => {},
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
) -> anyhow::Result<ReconcilePass> {
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
    // Every device peer is dialed by the first phase; one that fails or does not converge in a
    // phase drops out of the phases after it.
    let mut reached = vec![true; device_peers.len()];
    reconcile_account_logs(
        conn,
        endpoint,
        account,
        &device_peers,
        &mut reached,
        "account reconciliation",
    )
    .await;
    ensure_founder_incarnations(conn)?;
    // A newly authored founder incarnation must reach peers before their table manifests run.
    reconcile_account_logs(
        conn,
        endpoint,
        account,
        &device_peers,
        &mut reached,
        "incarnation propagation",
    )
    .await;
    reconcile_content_and_tables(conn, endpoint, account, &device_peers, &reached).await?;
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
    Ok(ReconcilePass {
        peers: reached.len() + resolved.unresolved_configured,
        converged: reached.iter().filter(|reached| **reached).count(),
    })
}

/// What one reconcile pass reached: every peer it tried (configured peers that never resolved
/// count as tried), and how many of them converged.
struct ReconcilePass {
    peers: usize,
    converged: usize,
}

/// One reconcile against `address` on this pass's clock and round cap.
async fn dial_and_reconcile<S: rag_rat_sync::SyncStore + NodeAuth>(
    endpoint: &iroh::Endpoint,
    address: rag_rat_sync::EndpointAddr,
    alpn: rag_rat_sync::SyncAlpn,
    store: &mut S,
    policy: AuthPolicy,
) -> Result<rag_rat_sync::ReconcileReport, rag_rat_sync::SyncFailure> {
    rag_rat_sync::connect_and_reconcile(
        endpoint,
        address,
        alpn,
        store,
        policy,
        time::now_ms,
        rag_rat_sync::MAX_RECONCILE_ROUNDS,
    )
    .await
}

/// Reconcile this account's log with every device peer still `reached`, clearing `reached` for a
/// peer that fails or does not converge. `phase` names the pass in its warnings.
async fn reconcile_account_logs(
    conn: &Connection,
    endpoint: &iroh::Endpoint,
    account: rag_rat_oplog::AccountId,
    device_peers: &[(String, rag_rat_sync::EndpointAddr)],
    reached: &mut [bool],
    phase: &'static str,
) {
    for ((peer, address), reached) in device_peers.iter().zip(reached.iter_mut()) {
        if !*reached {
            continue;
        }
        let mut store = OplogSyncStore::new(conn, account, time::now_ms);
        match dial_and_reconcile(
            endpoint,
            address.clone(),
            rag_rat_sync::SyncAlpn::Account,
            &mut store,
            AuthPolicy::Closed,
        )
        .await
        {
            Ok(report) if report.converged => {},
            Ok(_) => {
                tracing::warn!(peer, "device sync {phase} did not converge");
                *reached = false;
            },
            Err(error) => {
                tracing::warn!(peer, %error, "device sync {phase} failed");
                *reached = false;
            },
        }
    }
}

/// Reconcile memory content, drain it into the local tables, then reconcile the synced tables,
/// with every device peer whose account log converged. A failed leg is logged and the pass moves
/// on; only the local drain can fail it.
async fn reconcile_content_and_tables(
    conn: &Connection,
    endpoint: &iroh::Endpoint,
    account: rag_rat_oplog::AccountId,
    device_peers: &[(String, rag_rat_sync::EndpointAddr)],
    reached: &[bool],
) -> anyhow::Result<()> {
    for ((peer, address), _) in device_peers.iter().zip(reached).filter(|(_, reached)| **reached) {
        let mut content = OplogContentSyncStore::new(conn, account, time::now_ms);
        if let Err(error) = dial_and_reconcile(
            endpoint,
            address.clone(),
            rag_rat_sync::SyncAlpn::Content,
            &mut content,
            AuthPolicy::Closed,
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
    Ok(())
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

/// The routes to try for ONE foreign account, each a node paired with the relay that reaches it.
///
/// `subscribed` must be that account's own routes (`subscription_routing(conn, account)`).
/// Configured peers come first, through the configured relay; each locator route dials through the
/// relay its locator named — the shipped default relay is always present, so it can never act as an
/// override without silencing every locator relay.
///
/// Only an EXACT duplicate collapses: the same node, under any spelling (compared as raw node-id
/// bytes), through the same relay. A route never suppresses another that reaches the same node a
/// different way, so a dead or hostile relay recorded for a node cannot shadow the one that works.
pub fn foreign_pull_peers(
    configured_peers: &[String],
    configured_relay: &str,
    subscribed: &[(String, Option<String>)],
) -> Vec<(String, String)> {
    let configured = configured_peers.iter().map(|peer| (peer.as_str(), configured_relay));
    let from_locators = subscribed.iter().map(|(peer, relay)| {
        let relay = relay.as_deref().filter(|relay| !relay.trim().is_empty());
        (peer.as_str(), relay.unwrap_or(configured_relay))
    });
    let mut routes: Vec<(String, String)> = Vec::new();
    for (peer, relay) in configured.chain(from_locators) {
        let duplicate = routes.iter().any(|(known, known_relay)| {
            peer_identity(known) == peer_identity(peer) && known_relay.trim() == relay.trim()
        });
        if !duplicate {
            routes.push((peer.to_string(), relay.to_string()));
        }
    }
    routes
}

/// Each node the routes name, once, in first-seen order. The peer memo ranks NODES, and a node
/// reachable through several relays is still one node.
pub fn distinct_peers(routes: &[(String, String)]) -> Vec<String> {
    let mut peers: Vec<String> = Vec::new();
    for (peer, _) in routes {
        if !peers.iter().any(|known| peer_identity(known) == peer_identity(peer)) {
            peers.push(peer.clone());
        }
    }
    peers
}

/// Resolve nodes, in dial order, to addresses — one per route that names the node.
///
/// A node named by several routes through different relays is dialed through each in turn, so a
/// relay that does not reach it costs one failed attempt rather than the node. Both pull paths dial
/// through here, so a relay a locator recorded cannot be dropped at one call site while the other
/// keeps it. `order` may spell a node id differently from `routes` — the peer memo keeps whichever
/// spelling answered — so routes are matched by node identity; a node with no route dials through
/// `fallback_relay`.
pub fn route_addrs(
    order: &[String],
    routes: &[(String, String)],
    fallback_relay: &str,
) -> Vec<(String, Result<rag_rat_sync::EndpointAddr, String>)> {
    let mut resolved = Vec::new();
    for peer in order {
        let mut relays: Vec<&str> = routes
            .iter()
            .filter(|(route, _)| peer_identity(route) == peer_identity(peer))
            .map(|(_, relay)| relay.as_str())
            .collect();
        if relays.is_empty() {
            relays.push(fallback_relay);
        }
        for relay in relays {
            let addr = rag_rat_sync::peer_addr(peer, relay).map_err(|e| e.to_string());
            resolved.push((peer.clone(), addr));
        }
    }
    resolved
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
    let configured_relay = relay_url(config);
    for target in targets {
        let account_hex = hash::hex_lower(&target.to_bytes());
        // A local fault for one account (its routing or memo rows, its roster count) must not cost
        // the accounts after it their pull, or skip the drain below for content that already
        // landed — the isolation a failing peer already gets inside `pull_account_via_peers`.
        if let Err(error) =
            pull_foreign_account(config, conn, endpoint, target, &account_hex, &configured_relay)
                .await
        {
            tracing::warn!(
                account = %account_hex,
                %error,
                "cross-account pull failed; the next cadence retries"
            );
        }
    }
    // Materialize whatever landed, once for the whole pass (idempotent when nothing changed).
    crate::drain_synced_memory(conn)?;
    Ok(())
}

/// Pull one foreign `target` from the peers its routes name, and memoize the peer that answered.
async fn pull_foreign_account(
    config: &Config,
    conn: &Connection,
    endpoint: &iroh::Endpoint,
    target: rag_rat_oplog::AccountId,
    account_hex: &str,
    configured_relay: &str,
) -> anyhow::Result<()> {
    // The TARGET account's own routes only: a locator describes how to reach the owner its
    // repository subscribes to, so another subscription's routes are never tried for this
    // account, and never allowed to displace a route toward it.
    let routes = foreign_pull_peers(
        &config.sync.server_peers,
        configured_relay,
        &crate::memory_write::subscription_routing(conn, account_hex)?,
    );
    if routes.is_empty() {
        // Discovery cannot stand in: a foreign account's discovery tag derives from that
        // account's own secret, which only its own devices hold.
        tracing::warn!(
            account = %account_hex,
            "cross-account sync has an account to pull but no peer to pull it from: set \
             [sync] server_peers, or subscribe from a `.rag-rat-stream` that names its host"
        );
        return Ok(());
    }
    let memo_key = format!("{PULL_PEER_MEMO_PREFIX}{account_hex}");
    let order = ordered_pull_peers(conn, &memo_key, &distinct_peers(&routes))?;
    let ordered: Vec<(String, rag_rat_sync::EndpointAddr)> =
        route_addrs(&order, &routes, configured_relay)
            .into_iter()
            .filter_map(|(peer, addr)| match addr {
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
        let account_report = match dial_and_reconcile(
            endpoint,
            addr.clone(),
            rag_rat_sync::SyncAlpn::Account,
            &mut account_store,
            AuthPolicy::PublicRead,
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
        let content_report = match dial_and_reconcile(
            endpoint,
            addr.clone(),
            rag_rat_sync::SyncAlpn::Content,
            &mut content_store,
            AuthPolicy::PublicRead,
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
    let Some(last) = rag_rat_db::meta::read_meta_i64(conn, LAST_SYNC)? else {
        return Ok(true);
    };
    let now = time::now_ms();
    let interval_ms = i64::try_from(interval_secs).unwrap_or(i64::MAX).saturating_mul(1000);
    Ok(!within_window(last, now, interval_ms))
}

/// Whether a stamp taken at `stamp_ms` is still inside a `window_ms` window at `now_ms`. A stamp
/// AHEAD of `now_ms` (a backwards wall-clock step) is outside it, so a clock that moved backwards
/// makes the periodic work due instead of suppressing it until the clock catches up.
fn within_window(stamp_ms: i64, now_ms: i64, window_ms: i64) -> bool {
    stamp_ms <= now_ms && now_ms - stamp_ms < window_ms
}

fn record_sync(conn: &Connection) -> anyhow::Result<()> {
    Ok(rag_rat_db::meta::set_meta_i64(conn, LAST_SYNC, time::now_ms())?)
}

fn heartbeat(conn: &Connection) -> anyhow::Result<()> {
    Ok(rag_rat_db::meta::set_meta_i64(conn, RESIDENT_HEARTBEAT, time::now_ms())?)
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
#[path = "sync_driver_tests.rs"]
mod tests;
