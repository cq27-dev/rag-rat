//! The `rag-rat sync` command: local memory-stream authoring configuration plus the peer transport
//! driver (a persisted node identity today; `serve`/pairing land on top).

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use rag_rat_base::config::Config;
use rag_rat_base::{hash, locks, time};
use rag_rat_sync::{AuthPolicy, NodeAuth, OplogSyncStore};
use rusqlite::{Connection, params};
use zeroize::Zeroizing;

use crate::cli::{SyncArgs, SyncCommand};
use crate::{open_index, print_output};

/// How long `serve` waits for the database-scoped session lock before refusing to start — kept
/// short so a second `serve` (or a running device sync) fails fast rather than hanging.
const SERVE_SESSION_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `serve`'s one-time startup writes wait for the per-repo write lock (e.g. behind a
/// watcher pass) before giving up; the operator retries.
const SERVE_INIT_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn sync(config: &Config, args: &SyncArgs) -> anyhow::Result<()> {
    // `serve` is a long-running server that manages its own connection; it must run OUTSIDE the
    // command-wide repo write lock, which would otherwise block indexing, the watcher, and GC for
    // the entire life of the server.
    if let SyncCommand::Serve { once } = &args.command {
        return serve(config, *once);
    }
    let lock_repo = locks::write_lock_repo_id(config);
    let _lock = locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    let db = open_index(config)?;
    match &args.command {
        SyncCommand::Enable => {
            let enabled = db.sync_enable()?;
            print_output(&serde_json::json!({
                "status": if enabled { "enabled" } else { "already_enabled" },
                "repo_id": db.active_repo_id,
                "sealed_local_authoring": true,
                "transport_configured": false,
                "note": "subsequent local memory changes are sealed; transport is not configured",
            }))
        },
        SyncCommand::CatchUp { target } => {
            let report = db.sync_catch_up(*target)?;
            print_output(&serde_json::json!({
                "status": "caught_up",
                "repo_id": db.active_repo_id,
                "target": report.target.to_string(),
                "required": report.required,
                "already_covered": report.already_covered,
                "authored": report.authored,
                "keys_rotated": false,
                "pairing_performed": false,
                "transport_configured": false,
                "note": "existing live keys were re-wrapped without rotation; no enrollment, pairing, or transport occurred",
            }))
        },
        SyncCommand::Serve { .. } => unreachable!("serve is dispatched before the write lock"),
    }
}

/// The relay this invocation binds: `RAG_RAT_SYNC_RELAY` (ops/tests) overrides the configured
/// `[sync] relay_url`, which itself defaults to the shipped relay.
fn effective_relay_url(config: &Config) -> String {
    match std::env::var("RAG_RAT_SYNC_RELAY") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => config.sync.relay_url.clone(),
    }
}

/// Run a headless store-and-forward peer for this account's op log: bind the sync endpoint over the
/// configured relay and replicate the account log with peers the roster authorizes. Serves the
/// account log only — the wire is scoped to one stream per session, so content replication is a
/// separate stream a later slice adds. Runs until interrupted; `once` serves a single connection.
fn serve(config: &Config, once: bool) -> anyhow::Result<()> {
    let relay = effective_relay_url(config);
    // Hold a database-scoped session lock for the SERVER'S WHOLE LIFETIME. `sync_node_secret` is
    // store-global, so a second `serve` (or a colocated device-side sync) on the same database
    // would bind a SECOND endpoint advertising the same iroh node id — the two would race relay
    // registration and inbound connections. This rejects that. Crucially it does NOT block the
    // watcher / indexing / GC (they take the per-repo write lock, not this one), so only other sync
    // ENDPOINTS are excluded — exactly the collision to prevent.
    let _serve_lock = locks::WriteLock::acquire_sync_session_timeout(
        &config.database,
        SERVE_SESSION_LOCK_TIMEOUT,
    )?
    .ok_or_else(|| {
        anyhow!(
            "another sync session already holds this database's node identity (a `serve` peer or \
             a device sync is running); only one endpoint may run at a time"
        )
    })?;
    // Startup WRITES — the schema migration, the account read, and the first-run node-key mint —
    // under the per-repo write lock, BOUNDED here because the global session lock is already held
    // (the per-repo-while-global lock-ordering rule). `open_index` is the SANCTIONED open: it
    // refuses a missing index and migrates FORWARD-ONLY under the schema lock + gate; a bare
    // `schema::apply` would replay data migrations and skip the gate. The repo lock is RELEASED
    // before the accept loop so the watcher / GC aren't blocked for the server's life; the loop's
    // own ingests rely on SQLite's writer serialization instead.
    let lock_repo = locks::write_lock_repo_id(config);
    let repo_lock =
        locks::WriteLock::acquire_timeout(&config.database, &lock_repo, SERVE_INIT_LOCK_TIMEOUT)?
            .ok_or_else(|| {
            anyhow!("the index write lock is busy (another writer is mid-pass); retry `sync serve`")
        })?;
    let db = crate::open_index(config)?;
    let (account_id, node_key) = {
        let conn = db.connection();
        (existing_account_or_hint(conn)?, node_secret(conn)?)
    };
    drop(repo_lock);

    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async move {
        // `db` moved in; re-borrow the connection here so the loop can hold it for the run.
        let conn = db.connection();
        let endpoint = rag_rat_sync::build_endpoint(*node_key, &relay)
            .await
            .with_context(|| format!("binding the sync endpoint over relay {relay}"))?;
        // A private account admits only peers whose binding verifies against the roster (mutual),
        // so the serve peer must itself hold an EFFECTIVE (roster-current) device. Fail fast by
        // running its own binding through the roster authorization path — not just an is-empty
        // check: a device REMOVED from the roster still holds its local key and mints a nonempty
        // binding, but every `Closed` peer would reject it as `NotRosterDevice`. Self-authorization
        // fails closed for both "no local device" and "revoked", so the server never starts only to
        // silently reject every session.
        let local_node = *endpoint.id().as_bytes();
        let probe = OplogSyncStore::new(conn, account_id, time::now_ms);
        let probe_now = time::now_ms();
        let binding = probe.local_binding(&local_node, probe_now)?;
        if !probe.authorize(&binding, &local_node, probe_now)? {
            bail!(
                "this peer's device is not an effective member of the account (no enrolled \
                 device, or removed from the roster) — it cannot serve a private account; run \
                 `rag-rat sync enable` and enroll this peer"
            );
        }
        let policy = AuthPolicy::Closed;

        tracing::info!(node_id = %endpoint.id(), relay = %relay, "sync serve listening");
        // The dialable node identity must reach the operator on stdout: repository logging is off
        // by default, and a peer cannot connect without the node id (the relay is shared config).
        crate::print_output(&serde_json::json!({
            "status": "listening",
            "node_id": endpoint.id().to_string(),
            "relay": relay,
        }))?;

        // The database-scoped session lock taken at startup is still held for this whole loop
        // (released only when serve exits), keeping this database's node identity singular. A
        // per-connection ingest needs no further lock — SQLite serializes it against any watcher
        // write.
        //
        // ONE ctrl_c future for the whole loop: a fresh `ctrl_c()` per iteration would let a signal
        // delivered between iterations be swallowed by tokio's driver and missed.
        let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());
        loop {
            let mut store = OplogSyncStore::new(conn, account_id, time::now_ms);
            let outcome = tokio::select! {
                // `accept_and_sync` reads `now_ms` once a peer connects — the server may wait here
                // arbitrarily long, so the auth timestamp must not predate the connection.
                report = rag_rat_sync::accept_and_sync(&endpoint, &mut store, policy, time::now_ms)
                    => Some(report),
                _ = &mut shutdown => None,
            };
            match outcome {
                None => {
                    tracing::info!("interrupted; shutting down");
                    break;
                },
                Some(Ok(report)) => tracing::info!(
                    sent = report.entries_sent,
                    received = report.entries_received,
                    stored = report.entries_newly_stored,
                    "sync session complete"
                ),
                // In one-shot mode the session IS the command's result, so a failed session is a
                // failed command (scripted checks must see the non-zero exit). The long-running
                // server logs and moves on to the next peer instead.
                Some(Err(e)) if once => return Err(anyhow!("sync session failed: {e}")),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "sync session failed");
                    // A closed endpoint makes `accept` fail immediately and forever, so continuing
                    // would spin the loop at 100% CPU logging the same error — exit non-zero.
                    if endpoint.is_closed() {
                        return Err(anyhow!("sync endpoint closed: {e}"));
                    }
                },
            }
            // Cadence WAL fold (#818): a long-lived writer must checkpoint mid-lifetime or its
            // `-wal` sidecar grows unbounded (passive autocheckpoint is pinned off). Size-gated and
            // best-effort, mirroring the watcher's per-pass fold.
            db.fold_wal();
            if once {
                break;
            }
        }
        anyhow::Ok(())
    })
}

/// Resolve this store's EXISTING local account WITHOUT minting one. A serve peer must already be
/// enrolled; merely starting the server must never create account or device identity state as a
/// side effect (that is `sync enable`'s job). The "no account yet" case becomes an actionable
/// enrollment hint.
fn existing_account_or_hint(conn: &Connection) -> anyhow::Result<rag_rat_oplog::AccountId> {
    rag_rat_oplog::read_local_account(conn)?.context(
        "no local sync account — run `rag-rat sync enable` and enroll this peer before serving",
    )
}

/// Meta key for this index's persisted iroh node secret (the transport identity).
const NODE_SECRET_META_KEY: &str = "sync_node_secret";

/// This index's stable iroh node secret — the transport identity, minted once and persisted so the
/// node id stays the same across launches. It is deliberately distinct from the account device
/// signing key: rotating or losing one must not touch the other. Stored hex in the global
/// `index_meta`, plaintext like every other row in the unencrypted index; the in-memory copy is
/// returned in a `Zeroizing` wrapper so it is scrubbed when the caller drops it. `INSERT OR IGNORE`
/// plus a mandatory re-read makes a concurrent first-open adopt whichever key actually landed, so
/// two processes never end up with different node ids.
fn node_secret(conn: &Connection) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    // The hex forms of the secret are held in `Zeroizing` too, so the only lingering plaintext copy
    // is the one at rest in `index_meta` (the DB is unencrypted by design).
    if let Some(stored) = rag_rat_db::meta::read_meta(conn, NODE_SECRET_META_KEY)? {
        return decode_node_secret(&Zeroizing::new(stored));
    }
    let mut fresh = Zeroizing::new([0u8; 32]);
    // `getrandom::Error` implements `std::error::Error` only behind getrandom's `std` feature,
    // which is off here — format it directly rather than through `Context`.
    getrandom::fill(fresh.as_mut_slice())
        .map_err(|e| anyhow!("OS CSPRNG unavailable to mint the sync node key: {e}"))?;
    let hex = Zeroizing::new(hash::hex_lower(fresh.as_slice()));
    conn.execute("INSERT OR IGNORE INTO index_meta(key, value) VALUES (?1, ?2)", params![
        NODE_SECRET_META_KEY,
        hex.as_str()
    ])?;
    let stored = Zeroizing::new(
        rag_rat_db::meta::read_meta(conn, NODE_SECRET_META_KEY)?
            .context("sync node secret missing from index_meta immediately after mint")?,
    );
    decode_node_secret(&stored)
}

/// Decode a persisted node secret: exactly 64 hex chars → 32 bytes. A wrong length or a non-hex
/// character is a corrupt persisted key, surfaced rather than silently coerced.
fn decode_node_secret(hex: &str) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let hex = hex.trim();
    if hex.len() != 64 {
        bail!("persisted sync node secret is {} hex chars, expected 64", hex.len());
    }
    let mut out = Zeroizing::new([0u8; 32]);
    for (i, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        match ((pair[0] as char).to_digit(16), (pair[1] as char).to_digit(16)) {
            (Some(hi), Some(lo)) => out[i] = ((hi << 4) | lo) as u8,
            _ => bail!("persisted sync node secret is not valid hex"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rag_rat_base::hash;
    use rusqlite::Connection;

    use super::{decode_node_secret, node_secret};

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        conn
    }

    #[test]
    fn node_secret_is_minted_once_and_stable_across_calls() {
        let conn = schema_conn();
        let first = node_secret(&conn).unwrap();
        assert_ne!(*first, [0u8; 32], "a real key is minted, not left zeroed");
        let second = node_secret(&conn).unwrap();
        assert_eq!(*first, *second, "the persisted node secret is stable across calls");
    }

    #[test]
    fn node_secret_hex_round_trips_every_byte() {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        let hex = hash::hex_lower(&bytes);
        assert_eq!(hex.len(), 64);
        assert_eq!(*decode_node_secret(&hex).unwrap(), bytes);
    }

    #[test]
    fn decode_node_secret_rejects_wrong_length_and_non_hex() {
        assert!(decode_node_secret("abcd").is_err(), "too short is rejected");
        assert!(decode_node_secret(&"zz".repeat(32)).is_err(), "non-hex chars are rejected");
    }
}
