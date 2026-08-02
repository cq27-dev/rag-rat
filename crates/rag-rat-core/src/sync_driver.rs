//! Shared device-sync driver for the CLI fallback and the active MCP resident host.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use rag_rat_base::config::Config;
use rag_rat_base::{hash, locks, time};
use rag_rat_db::storage::IndexConnection;
use rag_rat_sync::{
    AuthPolicy, NodeAuth, OplogContentSyncStore, OplogSyncStore, PeerAuthorization, PeerCapability,
};
use rusqlite::{Connection, params};
use zeroize::Zeroizing;

const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const LAST_SYNC: &str = "sync_device_last_at_ms";
const RESIDENT_HEARTBEAT: &str = "sync_resident_heartbeat_at_ms";
const RESIDENT_NUDGE: &str = "sync_resident_nudge_at_ms";
const HEARTBEAT_MAX_AGE_MS: i64 = 30_000;
const HEARTBEAT_INTERVAL_MS: i64 = HEARTBEAT_MAX_AGE_MS / 2;
const NODE_SECRET: &str = "sync_node_secret";
/// Bound pre-auth peers as well as authenticated sessions: each task owns a SQLite connection
/// until the stream-idle timeout expires.
const RESIDENT_SESSION_MAX: usize = 8;

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
    let mut announcement = resident_announcement(&config, &endpoint, &database);
    let advertiser = announcement.as_ref().zip(discovery_addr(&config, &relay_url(&config))).map(
        |((tag, _, _, receiver, _), service)| {
            tokio::task::spawn_local(rag_rat_sync::discovery::advertise(
                rag_rat_sync::discovery::Advertise {
                    endpoint: endpoint.clone(),
                    service,
                    tag: *tag,
                    announcement: receiver.clone(),
                    ttl_seconds: rag_rat_sync::discovery::publish_ttl_seconds(
                        config.sync.push_interval_secs,
                    ),
                },
            ))
        },
    );
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
            refresh_announcement(conn, &mut announcement)?;
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
    if let Some(advertiser) = advertiser {
        advertiser.abort();
    }
}

type ResidentAnnouncement = (
    [u8; 32],
    [u8; 32],
    tokio::sync::watch::Sender<Option<Vec<u8>>>,
    tokio::sync::watch::Receiver<Option<Vec<u8>>>,
    Option<rag_rat_oplog::discovery::RosterStamp>,
);

fn resident_announcement(
    config: &Config,
    endpoint: &iroh::Endpoint,
    database: &std::path::Path,
) -> Option<ResidentAnnouncement> {
    if !config.sync.discovery || !config.sync.discoverable {
        return None;
    }
    let storage = match IndexConnection::open(database) {
        Ok(storage) => storage,
        Err(error) => {
            tracing::warn!(%error, "could not open the resident sync announcement store");
            return None;
        },
    };
    let conn = storage.connection();
    let tag = match rag_rat_sync::discovery::discovery_secret(conn) {
        Ok(Some(secret)) => rag_rat_sync::discovery::account_tag(&secret),
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, "could not read the resident sync discovery secret");
            return None;
        },
    };
    let (bytes, seal_succeeded) = match seal_announcement(conn, &tag, endpoint.id().as_bytes()) {
        Ok(bytes) => (bytes, true),
        Err(error) => {
            tracing::warn!(%error, "could not seal the resident sync announcement");
            (None, false)
        },
    };
    // A failed initial seal must leave this unset so the periodic refresh retries against the same
    // roster rather than treating an unpublished announcement as current.
    let stamp = if seal_succeeded {
        rag_rat_oplog::discovery::roster_stamp(conn).ok().flatten()
    } else {
        None
    };
    let (sender, receiver) = tokio::sync::watch::channel(bytes);
    Some((tag, *endpoint.id().as_bytes(), sender, receiver, stamp))
}

fn refresh_announcement(
    conn: &Connection,
    announcement: &mut Option<ResidentAnnouncement>,
) -> anyhow::Result<()> {
    let Some((tag, node, sender, _, stamp)) = announcement else {
        return Ok(());
    };
    let observed = rag_rat_oplog::discovery::roster_stamp(conn)?;
    if observed == *stamp {
        return Ok(());
    }
    match seal_announcement(conn, tag, node) {
        Ok(bytes) => {
            *stamp = observed;
            let _ = sender.send(bytes);
        },
        Err(error) => {
            tracing::warn!(%error, "could not refresh the resident sync announcement");
            let _ = sender.send(None);
        },
    }
    Ok(())
}

fn seal_announcement(
    conn: &Connection,
    tag: &[u8; 32],
    node: &[u8; 32],
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(sealed) = rag_rat_oplog::discovery::seal_discovery_announcement(conn, tag, node)?
    else {
        return Ok(None);
    };
    if sealed.recipients <= 1
        || sealed.bytes.len() > rag_rat_sync::discovery::MAX_ANNOUNCEMENT_BYTES
    {
        return Ok(None);
    }
    Ok(Some(sealed.bytes))
}

async fn accept_loop(
    endpoint: iroh::Endpoint,
    account: rag_rat_oplog::AccountId,
    database: PathBuf,
) {
    let sessions = Arc::new(tokio::sync::Semaphore::new(RESIDENT_SESSION_MAX));
    loop {
        let connection = match rag_rat_sync::accept_connection(&endpoint).await {
            Ok(connection) => connection,
            Err(error) if endpoint.is_closed() => {
                tracing::warn!(%error, "resident sync endpoint closed");
                return;
            },
            Err(error) => {
                tracing::warn!(%error, "resident sync accept failed");
                continue;
            },
        };
        let Ok(permit) = Arc::clone(&sessions).try_acquire_owned() else {
            // Do not queue unauthenticated peers behind the session limit. Their connections can
            // wait out the stream timeout otherwise, consuming endpoint and OS resources.
            connection.close(0u32.into(), b"session-limit");
            continue;
        };
        let database = database.clone();
        let node = *endpoint.id().as_bytes();
        tokio::task::spawn_local(async move {
            let _permit = permit;
            let result = async {
                let storage = IndexConnection::open(&database)?;
                let conn = storage.connection();
                let mut account_store = OplogSyncStore::new(conn, account, time::now_ms);
                let mut content_store = OplogContentSyncStore::new(conn, account, time::now_ms);
                let (alpn, report) = rag_rat_sync::dispatch_connection(
                    connection,
                    node,
                    &mut account_store,
                    &mut content_store,
                    AuthPolicy::Closed,
                    time::now_ms,
                )
                .await?;
                if alpn.as_slice() == rag_rat_sync::CONTENT_SYNC_ALPN {
                    crate::drain_synced_memory(conn)?;
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

    use rag_rat_base::config::Config;
    use rusqlite::Connection;

    use super::{
        DeviceSyncOutcome, RESIDENT_NUDGE, can_host, can_sync, device_sync_run, nudge_resident_host,
    };

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
}
