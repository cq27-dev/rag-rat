//! The `rag-rat sync` command: local memory-stream authoring configuration plus the peer transport
//! driver (a persisted node identity today; `serve`/pairing land on top).

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use rag_rat_base::config::Config;
use rag_rat_base::{hash, locks, time};
use rag_rat_sync::{
    AuthPolicy, NodeAuth, OplogContentSyncStore, OplogSyncStore, PeerAuthorization, PeerCapability,
};
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
    // `serve`, `init`, and `join` bind an endpoint and run their own network loop; they must run
    // OUTSIDE the command-wide repo write lock (which would block indexing, the watcher, and GC for
    // the server's whole life) and manage the database-scoped SESSION lock themselves instead.
    match &args.command {
        SyncCommand::Serve { once } => return serve(config, *once),
        SyncCommand::Init { role, label, ttl_secs } =>
            return init(config, InviteMint {
                role: role.to_device_role(),
                label: label.clone(),
                ttl: Duration::from_secs(*ttl_secs),
            }),
        SyncCommand::Join { ticket } => return join(config, ticket),
        SyncCommand::Enable | SyncCommand::CatchUp { .. } => {},
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
        SyncCommand::Serve { .. } | SyncCommand::Init { .. } | SyncCommand::Join { .. } =>
            unreachable!("the network commands are dispatched before the write lock"),
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

/// The peer-discovery service this invocation queries: `RAG_RAT_SYNC_DISCOVERY_NODE` (ops/tests)
/// overrides the configured `[sync] discovery_node_id`, which itself defaults to the shipped
/// service. A NODE ID, not a URL — the service is a separate iroh peer reached through the relay.
fn effective_discovery_node(config: &Config) -> String {
    discovery_node_or_configured(
        std::env::var("RAG_RAT_SYNC_DISCOVERY_NODE").ok().as_deref(),
        config,
    )
}

/// The precedence rule behind [`effective_discovery_node`], with the environment read lifted out.
///
/// Split so the rule is testable without mutating process-global state: env-var tests are
/// invisible under a per-process test runner and race under an in-process one, and this repository
/// is verified under both.
fn discovery_node_or_configured(from_env: Option<&str>, config: &Config) -> String {
    match from_env {
        Some(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => config.sync.discovery_node_id.clone(),
    }
}

/// A one-time enrollment invite `sync init` mints as it starts hosting the pairing exchange.
struct InviteMint {
    role: rag_rat_oplog::DeviceRole,
    label: Option<String>,
    ttl: Duration,
}

/// Run a headless store-and-forward peer for this account's op log: bind the sync endpoint over the
/// configured relay and replicate with peers the roster authorizes. Serves BOTH streams a peer may
/// negotiate — the account log (`SYNC_ALPN`) and `/3` content (`CONTENT_SYNC_ALPN`) — routing
/// each connection by its ALPN. Runs until interrupted; `once` serves a single connection (one
/// stream), so a full account+content sync a device drives needs two connections.
fn serve(config: &Config, once: bool) -> anyhow::Result<()> {
    serve_with(config, once, None)
}

/// Owner-side pairing (`sync init`): mint a one-time invite, print the ticket, then host the
/// enrollment exchange AND the joiner's follow-up account + content restore until interrupted. It
/// is the same accept loop as [`serve`] — `accept_and_dispatch` already routes the enrollment ALPN
/// against the owner's database — so `init` is simply "mint, then serve". Never `--once`: a joiner
/// needs the enrollment connection plus two sync connections.
fn init(config: &Config, mint: InviteMint) -> anyhow::Result<()> {
    serve_with(config, false, Some(mint))
}

/// Shared machinery behind [`serve`] and [`init`]: acquire the session lock, open the index, bind
/// the endpoint, and run the ALPN-dispatching accept loop. When `mint` is set (`sync init`), a
/// one-time invite is minted AFTER the endpoint binds and the roster gate passes — so a bind
/// failure never strands the candidate reservation the mint makes — and its ticket is printed on
/// the startup line. Minting enforces founder/owner authority, so a non-owner `init` fails there.
fn serve_with(config: &Config, once: bool, mint: Option<InviteMint>) -> anyhow::Result<()> {
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
        // so the serve peer must itself hold an EFFECTIVE (roster-current) device. Fail fast rather
        // than start and silently reject every session.
        let local_node = *endpoint.id().as_bytes();
        if !device_can_serve(device_roster_capability(conn, account_id, &local_node)?) {
            bail!(
                "this peer's device is not a write-capable effective member of the account \
                 (unenrolled, removed, or read-only) — it cannot serve a private account; enroll \
                 this peer with the Member or Owner role"
            );
        }
        // `sync init` mints its one-time invite ONLY NOW — after the endpoint bound and the roster
        // gate passed — so a bind failure (e.g. a bad relay) never strands the candidate
        // reservation the mint makes, which would otherwise gate the next mint until the
        // TTL expires. The mint is a single BEGIN IMMEDIATE transaction serialized by
        // SQLite against any watcher write (the same lock-free posture as the accept loop's
        // ingests below), so it needs no repo write lock. Minting enforces founder/owner
        // authority; a non-owner `init` fails here.
        let invite = match mint {
            Some(mint) => {
                let ticket = rag_rat_sync::mint_invite(conn, rag_rat_sync::InviteSpec {
                    account_id,
                    inviter_node_id: local_node,
                    relay_url: relay.clone(),
                    role: mint.role,
                    label: mint.label.as_deref(),
                    now_ms: &|| time::now_ms(),
                    ttl: mint.ttl,
                })
                .map_err(|e| anyhow!("minting the enrollment invite failed: {e}"))?;
                Some((ticket, mint.role))
            },
            None => None,
        };
        let policy = AuthPolicy::Closed;

        // Advertise this host to the account's other devices for as long as it serves.
        //
        // `serve` is the node that most needs announcing — it is the always-on peer a laptop
        // behind NAT is trying to reach — and it is the one node the device-sync path can never
        // announce, because that path only runs on the maintenance hook and `serve` has no
        // maintenance hook. Without its own timer the only publishers would be the only fetchers.
        //
        // Not gated on the roster count the device path uses: `serve` outlives the roster it
        // started with, and a host that stopped advertising because it was alone when it booted
        // would be invisible to the device enrolled an hour later. `--once` does not publish — it
        // is a scripted single-connection check, not a host.
        let advertising =
            serve_should_advertise(config.sync.discovery, config.sync.discoverable, once);
        let discovery_tag = advertising
            .then(|| rag_rat_sync::discovery::discovery_secret(conn))
            .transpose()?
            .flatten()
            .map(|secret| rag_rat_sync::discovery::account_tag(&secret));
        // Seal ONCE per roster change, not once per publish, and hand the advertiser the bytes.
        //
        // Sealing needs a connection; the advertiser is a spawned task and cannot hold one. So this
        // loop — which already owns the connection and already folds the WAL between sessions — is
        // where sealing happens, and the watch carries the result. Republishing identical bytes is
        // safe (same key, same nonce, same plaintext) and buys two things: the advertiser stays
        // database-free, and `is_live` can recognise this host's own announcement by comparing
        // bytes rather than opening anything.
        let mut announcement = discovery_tag.map(|tag| {
            let sealed = seal_announcement(conn, &tag, &local_node);
            let stamp = sealed.as_ref().map(|(_, stamp)| *stamp);
            let (tx, rx) = tokio::sync::watch::channel(sealed.map(|(bytes, _)| bytes));
            (tag, tx, rx, stamp)
        });
        // Aborted on the way out of this scope, so the loop cannot outlive the endpoint it
        // advertises.
        let _advertiser = announcement
            .as_ref()
            .map(|(tag, _, rx, _)| (*tag, rx.clone()))
            .zip(discovery_service_addr(config, &relay))
            .map(|((tag, announcement), service)| {
                AbortOnDrop(tokio::spawn(rag_rat_sync::discovery::advertise(
                    rag_rat_sync::discovery::Advertise {
                        endpoint: endpoint.clone(),
                        service,
                        tag,
                        announcement,
                        ttl_seconds: rag_rat_sync::discovery::publish_ttl_seconds(
                            config.sync.push_interval_secs,
                        ),
                        now_ms: time::now_ms,
                    },
                )))
            });

        tracing::info!(
            node_id = %endpoint.id(),
            relay = %relay,
            minted_invite = invite.is_some(),
            discoverable = config.sync.discoverable,
            "sync serve listening"
        );
        // The dialable node identity must reach the operator on stdout: repository logging is off
        // by default, and a peer cannot connect without the node id (the relay is shared config).
        // `sync init` additionally emits the invite ticket to share with the joining device.
        let mut listening = serde_json::json!({
            "status": "listening",
            "node_id": endpoint.id().to_string(),
            "relay": relay,
        });
        if let Some((ticket, role)) = &invite {
            listening["invite"] = serde_json::json!(ticket.to_ticket_string());
            listening["invite_role"] = serde_json::json!(role.as_db_str());
            listening["invite_expires_at_ms"] = serde_json::json!(ticket.expires_at_ms);
        }
        crate::print_output(&listening)?;

        // The database-scoped session lock taken at startup is still held for this whole loop
        // (released only when serve exits), keeping this database's node identity singular. A
        // per-connection ingest needs no further lock — SQLite serializes it against any watcher
        // write.
        //
        // ONE ctrl_c future for the whole loop: a fresh `ctrl_c()` per iteration would let a signal
        // delivered between iterations be swallowed by tokio's driver and missed.
        let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());
        loop {
            // One endpoint, one accept loop: a connection carries the ACCOUNT-LOG ALPN or the
            // CONTENT ALPN, and `accept_and_dispatch` routes it to the matching store after the
            // (account-level) auth phase. Both stores are cheap handles reconstructed per accept.
            let mut account_store = OplogSyncStore::new(conn, account_id, time::now_ms);
            let mut content_store = OplogContentSyncStore::new(conn, account_id, time::now_ms);
            let outcome = tokio::select! {
                // `accept_and_dispatch` reads `now_ms` once a peer connects — the server may wait
                // here arbitrarily long, so the auth timestamp must not predate the connection.
                result = rag_rat_sync::accept_and_dispatch(
                    &endpoint,
                    &mut account_store,
                    &mut content_store,
                    policy,
                    time::now_ms,
                ) => Some(result),
                _ = &mut shutdown => None,
            };
            match outcome {
                None => {
                    tracing::info!("interrupted; shutting down");
                    break;
                },
                Some(Ok((alpn, report))) => tracing::info!(
                    stream = %String::from_utf8_lossy(&alpn),
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
            // Re-seal between sessions, where the roster can have moved: an accept-loop ingest is
            // the only way it changes under a serving host, since the session lock excludes a
            // colocated device sync and no command authors roster changes. A device enrolled while
            // this host is running therefore becomes a recipient at the next renewal; the
            // advertiser re-reads the watch every tick, so the update needs no signal beyond this.
            if let Some((tag, tx, _, stamp)) = &mut announcement {
                // Compare the ROSTER, not the envelope. Sealing draws a fresh ephemeral per wrap,
                // so two seals of an unchanged roster differ in every byte — comparing envelopes
                // would re-seal after every session, the advertiser would stop recognising its own
                // live announcement, and it would republish on every tick until the tag was full of
                // its own copies. That is precisely the failure seal-once exists to avoid.
                let current = rag_rat_oplog::discovery::roster_stamp(conn).unwrap_or_else(|error| {
                    tracing::warn!(%error, "could not read the roster; keeping the current announcement");
                    *stamp
                });
                // No test reaches this comparison: `serve_with` binds an endpoint, takes the
                // session lock, and loops until interrupted. **Replacing the condition with `true`
                // survives the whole suite** — it re-seals every session, which is the bug this
                // stamp exists to prevent. What IS pinned, in the op-log crate, is the pair of
                // facts that make the bug possible and the fix correct: two seals of an unchanged
                // roster differ in every byte, and the stamp does not.
                if current != *stamp {
                    tracing::debug!("the roster moved; re-sealing this host's announcement");
                    let resealed = seal_announcement(conn, tag, &local_node);
                    *stamp = resealed.as_ref().map(|(_, stamp)| *stamp);
                    let _ = tx.send(resealed.map(|(bytes, _)| bytes));
                }
            }
            if once {
                break;
            }
        }
        anyhow::Ok(())
    })
}

/// Joiner-side pairing (`sync join`): redeem an invite ticket, enrolling THIS device into the
/// account, then restore its state (account log then `/3` content) from the inviter. Holds the
/// database-scoped session lock for the whole exchange — enrollment + restore consume candidate
/// capacity, which must be serialized against any colocated `serve`/device sync (the requirement
/// `connect_and_enroll` documents).
fn join(config: &Config, ticket: &str) -> anyhow::Result<()> {
    let ticket = rag_rat_sync::EnrollmentTicket::from_ticket_string(ticket)
        .map_err(|e| anyhow!("invalid enrollment ticket: {e}"))?;
    // Bind over the INVITER's relay (the ticket's), not the local `[sync] relay_url`: both peers
    // must share a relay to meet, and the ticket names where the inviter is reachable. `sync init`
    // minted the ticket with the relay IT is serving on.
    let relay = ticket.relay_url.clone();
    let _session = locks::WriteLock::acquire_sync_session_timeout(
        &config.database,
        SERVE_SESSION_LOCK_TIMEOUT,
    )?
    .ok_or_else(|| {
        anyhow!(
            "another sync session already holds this database's node identity (a `serve` peer or \
             a device sync is running); stop it before joining"
        )
    })?;
    let lock_repo = locks::write_lock_repo_id(config);
    let repo_lock =
        locks::WriteLock::acquire_timeout(&config.database, &lock_repo, SERVE_INIT_LOCK_TIMEOUT)?
            .ok_or_else(|| {
            anyhow!("the index write lock is busy (another writer is mid-pass); retry `sync join`")
        })?;
    let db = crate::open_index(config)?;
    let node_key = {
        let conn = db.connection();
        // A store already bound to a DIFFERENT account cannot adopt this ticket — enrollment would
        // corrupt its identity. A matching account is allowed (a resumed or repeated join).
        if let Some(existing) = rag_rat_oplog::read_local_account(conn)?
            && existing != ticket.account_id
        {
            bail!(
                "this store already belongs to a different sync account; `sync join` enrolls a \
                 fresh device"
            );
        }
        // Mint the account device identity if absent (NOT a genesis) so the enrollment request can
        // present the joiner's keys. `local_device` is idempotent on an existing identity.
        rag_rat_oplog::local_device(conn, time::now_ms())?;
        node_secret(conn)?
    };
    drop(repo_lock);

    let account_id = ticket.account_id;
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async move {
        let conn = db.connection();
        let endpoint = rag_rat_sync::build_endpoint(*node_key, &relay)
            .await
            .with_context(|| format!("binding the sync endpoint over relay {relay}"))?;
        let peer = rag_rat_sync::peer_addr_from_bytes(&ticket.inviter_node_id, &relay)
            .map_err(|e| anyhow!("the ticket's inviter address is invalid: {e}"))?;
        let local_node = *endpoint.id().as_bytes();
        // Whether this device is ALREADY a roster member — a resumed join whose enrollment
        // committed on an earlier run. It decides only whether restore may still proceed if
        // redemption fails.
        let already_effective = device_roster_capability(conn, account_id, &local_node)?.is_some();

        // 1) Enrollment — dial the owner over the enroll ALPN. The owner authors this device's
        //    DeviceAdd and returns the account bootstrap, which `connect_and_enroll` adopts,
        //    leaving this device roster-effective. Budget / held-entries / transport id are
        //    recomputed from the local store inside the call, so the placeholders here are
        //    overwritten.
        //
        //    ALWAYS attempt redemption — never skip on an already-effective device. Within the
        //    receipt-replay window the owner replays the exact prior receipt and adoption is
        //    idempotent, so a resumed join re-runs cleanly; and a genuinely UNUSED ticket is
        //    consumed rather than left live for another bearer. Only a consumed/expired/unknown
        //    nonce on a device that is ALREADY enrolled falls back to restore (a resume past the
        //    replay window, where enrollment is unnecessary); every other failure — and any failure
        //    on a not-yet-enrolled device — is a real error.
        let local = rag_rat_oplog::local_device(conn, time::now_ms())?;
        let request = rag_rat_sync::EnrollmentRequest {
            nonce: ticket.nonce,
            expected_account: account_id,
            ed25519_pubkey: local.ed25519_public_key(),
            x25519_pubkey: local.x25519_public_key(),
            transport_node_id: [0u8; 32],
            budget: rag_rat_oplog::EnrollmentBudget {
                account_entries_remaining: 0,
                account_bytes_remaining: 0,
                global_entries_remaining: 0,
                global_bytes_remaining: 0,
            },
            held_entry_hashes: Vec::new(),
        };
        match rag_rat_sync::connect_and_enroll(
            &endpoint,
            peer.clone(),
            conn,
            account_id,
            &request,
            time::now_ms(),
        )
        .await
        {
            Ok(_) => {},
            Err(error)
                if already_effective
                    && matches!(
                        error,
                        rag_rat_sync::InviteError::Used
                            | rag_rat_sync::InviteError::Expired
                            | rag_rat_sync::InviteError::Unknown
                    ) =>
                tracing::info!(
                    %error,
                    "invite already spent and this device is enrolled; resuming restore"
                ),
            Err(error) => return Err(anyhow!("enrollment failed: {error}")),
        }

        // 2) Restore-from-zero — pull the account log then `/3` content from the inviter, now that
        //    this device is roster-effective. Content rides on the account log's authority, so the
        //    account log runs first. Each stream reconciles to a fixpoint (#878): a single session
        //    can report `Done` while the store is still incomplete, so re-run until dry (or the
        //    round cap). The inviter must still be serving (its `sync init` / `serve`).
        let account_report = {
            let mut store = OplogSyncStore::new(conn, account_id, time::now_ms);
            rag_rat_sync::connect_and_reconcile(
                &endpoint,
                peer.clone(),
                rag_rat_sync::SYNC_ALPN,
                &mut store,
                AuthPolicy::Closed,
                time::now_ms,
                rag_rat_sync::MAX_RECONCILE_ROUNDS,
            )
            .await
            .map_err(|e| anyhow!("restoring the account log from the inviter failed: {e}"))?
        };
        let content_report = {
            let mut store = OplogContentSyncStore::new(conn, account_id, time::now_ms);
            rag_rat_sync::connect_and_reconcile(
                &endpoint,
                peer,
                rag_rat_sync::CONTENT_SYNC_ALPN,
                &mut store,
                AuthPolicy::Closed,
                time::now_ms,
                rag_rat_sync::MAX_RECONCILE_ROUNDS,
            )
            .await
            .map_err(|e| anyhow!("restoring content from the inviter failed: {e}"))?
        };
        db.fold_wal();

        // The inviter's node id in dial form, so the operator can add it to `[sync] server_peers`
        // for ongoing sync (auto-persisting it is a deliberate follow-up). The inviter's RELAY must
        // travel with it: device-side sync rebuilds the peer address from `[sync] relay_url`, so if
        // the inviter serves on a different relay than this device's default, ongoing sync would
        // dial the wrong relay unless the operator also points `relay_url` at the inviter's.
        let inviter = rag_rat_sync::node_id_to_string(&ticket.inviter_node_id)
            .unwrap_or_else(|_| hash::hex_lower(&ticket.inviter_node_id));
        crate::print_output(&serde_json::json!({
            "status": "joined",
            "account_id": hash::hex_lower(&account_id.to_bytes()),
            "account_entries_restored": account_report.entries_newly_stored,
            "content_entries_restored": content_report.entries_newly_stored,
            // `converged=false` means the restore hit the reconciliation round cap and a later
            // device-side sync should continue — the account/content stores may not be complete yet.
            "converged": account_report.converged && content_report.converged,
            "inviter_node_id": inviter,
            "inviter_relay": ticket.relay_url,
            "note": "to keep syncing, add inviter_node_id to [sync] server_peers and set [sync] \
                     relay_url (or RAG_RAT_SYNC_RELAY) to inviter_relay",
        }))?;
        anyhow::Ok(())
    })
}

/// How long device-side sync waits for the per-database session lock before deferring to the next
/// maintenance pass. Kept short: a colocated `serve` peer holds this lock for its whole life, so a
/// long wait would just burn time on every hook before deferring; a transient overlap with another
/// device sync resolves on the next trigger anyway (the cadence has already been satisfied).
const DEVICE_SYNC_LOCK_TIMEOUT: Duration = Duration::from_secs(1);
/// Meta key: the last device-side sync attempt (ms), the cadence watermark.
const DEVICE_SYNC_LAST_META_KEY: &str = "sync_device_last_at_ms";

/// What a device-side sync attempt did — folded into the maintenance hook report.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeviceSyncOutcome {
    /// Nothing to do: no local account, or this device is not roster-effective. Note that having
    /// no configured peer is NOT one of the reasons — a pass with an empty `server_peers` still
    /// runs and still looks.
    Disabled,
    /// The cadence gate suppressed this attempt (a sync ran within `push_interval_secs`).
    Skipped,
    /// The per-database session lock is held (a serve peer or another sync); retry next pass.
    Deferred,
    /// Ran against the peers this pass resolved — configured plus discovered: `ok` sessions
    /// succeeded, `errors` failed, and `ok + errors == peers`.
    ///
    /// `peers` is what was ATTEMPTED, not what was configured. The two diverge in both directions:
    /// discovery adds peers no config named, and several configured spellings of one node collapse
    /// into a single dial. A configured id that failed to resolve is logged and counted as an
    /// error, so a typo stays visible rather than shrinking the peer set to a healthy-looking zero.
    Ran { peers: usize, ok: usize, errors: usize },
}

/// Best-effort device-side account-log sync on the maintenance path: dial each configured server
/// peer and run one bidirectional session (push the entries the peer lacks, pull the ones we lack).
/// Reuses the migrated `conn` from the maintenance pass and never holds the repo write lock — each
/// account ingest is a single SQLite transaction serialized against any watcher write. Every
/// per-peer failure is counted and logged; the caller folds the outcome into the hook report and a
/// broken peer never fails the hook.
pub(crate) fn device_sync_run(
    config: &Config,
    conn: &Connection,
) -> anyhow::Result<DeviceSyncOutcome> {
    // Device-side sync must NOT mint identity: an unenrolled store simply has nothing to sync.
    // Read the account FIRST — ahead of the configured-peers check it used to sit behind — because
    // whether an empty `server_peers` means "nothing to do" is now an account-scoped question.
    let Some(account_id) = rag_rat_oplog::read_local_account(conn)? else {
        return Ok(DeviceSyncOutcome::Disabled);
    };
    // An enrolled account is reason enough to run: this pass may have nothing configured and know
    // of no other device, and still need to fetch.
    //
    // Deliberately NOT gated on the local roster holding a second device. That count is REPLICATED
    // state — it is only current if this device has synced recently — so using it to decide whether
    // to sync is circular, and the circle closes badly: a store restored from a backup taken before
    // the other devices were enrolled believes it is alone, declines to look, and therefore never
    // receives the roster entries that would tell it otherwise. It stays wedged forever while a
    // reachable host sits there advertising. The cost of getting this wrong in the permissive
    // direction is one fetch per cadence for an account that really is alone; in the strict
    // direction it is a device that can never recover.
    if !device_sync_due(conn, config.sync.push_interval_secs)? {
        return Ok(DeviceSyncOutcome::Skipped);
    }
    // The per-database session lock: this device-side ephemeral session must not run while a
    // colocated serve peer (or another device sync) holds the same node identity. On timeout,
    // defer to the next maintenance pass rather than block the hook.
    let Some(_session) =
        locks::WriteLock::acquire_sync_session_timeout(&config.database, DEVICE_SYNC_LOCK_TIMEOUT)?
    else {
        return Ok(DeviceSyncOutcome::Deferred);
    };
    // Re-check the cadence UNDER the lock (double-checked locking): several git hooks fire per
    // action and can all pass the pre-lock check on the same old watermark; without this a
    // serialized contender would re-dial every peer right after the first run stamps the watermark.
    if !device_sync_due(conn, config.sync.push_interval_secs)? {
        return Ok(DeviceSyncOutcome::Skipped);
    }
    let node_key = node_secret(conn)?;
    // Gate on roster-effectiveness BEFORE binding an endpoint: the node id is derivable from the
    // secret, so this pays no socket or relay traffic. A revoked or unenrolled device could
    // authorize to no `Closed` peer, so dialing would only fail every session. Stamp the watermark
    // so this local-broken state is cadence-limited exactly like an unreachable peer — otherwise
    // every hook would rebind an endpoint and hit the relay for nothing.
    let local_node = rag_rat_sync::node_id_from_secret(*node_key);
    if !device_can_sync(device_roster_capability(conn, account_id, &local_node)?) {
        record_device_sync(conn)?;
        return Ok(DeviceSyncOutcome::Disabled);
    }
    let relay = effective_relay_url(config);
    // Key material for the account's discovery tag. Absent only when no account is minted, which
    // the read above already ruled out; treat it as "no discovery" rather than an error either way.
    let discovery_tag = rag_rat_sync::discovery::discovery_secret(conn)?
        .map(|secret| rag_rat_sync::discovery::account_tag(&secret));
    let discovery_service = discovery_service_addr(config, &relay);

    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result: anyhow::Result<(usize, usize, usize)> = runtime.block_on(async {
        let endpoint = rag_rat_sync::build_endpoint(*node_key, &relay)
            .await
            .with_context(|| format!("binding the sync endpoint over relay {relay}"))?;
        // Resolve which peers to dial: the configured ids plus whatever the account advertises.
        // INSIDE the runtime and AFTER the bind on purpose — publishing advertises this endpoint's
        // identity, so there is nothing to announce until it exists. Each entry pairs the peer's
        // node-id string (for logs) with its dialable address; a configured id that does not parse
        // is logged and counted there, so it never becomes a dial attempt.
        let resolved = rag_rat_sync::discover_peers(
            &config.sync.server_peers,
            &relay,
            discovery_tag.zip(discovery_service).map(|(tag, service)| {
                rag_rat_sync::discovery::DiscoveryExchange {
                    endpoint: &endpoint,
                    service,
                    tag,
                    // FETCH ONLY, whatever `[sync] discoverable` says — that flag is honoured by
                    // `serve_with`, which is a different process state.
                    //
                    // This pass never calls `accept_and_dispatch`: it dials out and drops the
                    // endpoint when it finishes, seconds later. Advertising here would publish an
                    // address that cannot accept a connection and stops existing almost
                    // immediately, while the announcement lives on for its whole TTL — costing
                    // every device that discovers it a dial that can only time out, and occupying
                    // one of the few per-tag slots that a REACHABLE peer needs. Only a node that
                    // accepts connections is worth announcing.
                    publish: None,
                    ttl_seconds: rag_rat_sync::discovery::publish_ttl_seconds(
                        config.sync.push_interval_secs,
                    ),
                    now_ms: time::now_ms(),
                }
            }),
            // Opening happens here, where a connection is available. An announcement sealed to a
            // roster this device is no longer on simply will not open, and is dropped with the
            // malformed ones — a device that has been removed stops finding its former peers by
            // discovery, which is the point of sealing them.
            &|payload| {
                discovery_tag.and_then(|tag| {
                    rag_rat_oplog::discovery::open_discovery_announcement(conn, &tag, payload)
                        .unwrap_or_else(|error| {
                            tracing::warn!(%error, "could not open a discovered announcement");
                            None
                        })
                })
            },
        )
        .await;
        let peers = resolved.peers;
        let mut reached = Vec::with_capacity(peers.len());
        for (peer, addr) in &peers {
            // Own a copy of the resolved address: it is dialed twice (account log, then content),
            // and `addr` is a borrow into `peers`.
            let addr = (*addr).clone();
            // Account log FIRST — it carries the roster + stream ownership that AUTHORIZE content,
            // so leading with it minimizes parking on the peer. The account-log result IS the
            // peer's outcome — exactly one entry in `reached` per peer. `/3` content rides on top:
            // best-effort, attempted only once the account log reached the peer, and a
            // content hiccup is logged — never a peer failure.
            // Reconcile each stream to a fixpoint (#878): a single session can report `Done` while
            // the store is still incomplete (adversarial dependent-before-authorizer ordering, or a
            // large active account), so re-run until a round is dry or the round cap is hit. A
            // capped (non-converged) pass just defers the remainder to the next
            // cadence.
            let account_ok = {
                let mut store = OplogSyncStore::new(conn, account_id, time::now_ms);
                match rag_rat_sync::connect_and_reconcile(
                    &endpoint,
                    addr.clone(),
                    rag_rat_sync::SYNC_ALPN,
                    &mut store,
                    AuthPolicy::Closed,
                    time::now_ms,
                    rag_rat_sync::MAX_RECONCILE_ROUNDS,
                )
                .await
                {
                    Ok(report) => {
                        tracing::info!(
                            peer,
                            rounds = report.rounds,
                            converged = report.converged,
                            sent = report.entries_sent,
                            stored = report.entries_newly_stored,
                            "device sync (account log) complete"
                        );
                        true
                    },
                    Err(e) => {
                        tracing::warn!(peer, error = %e, "device sync (account log) failed");
                        false
                    },
                }
            };
            if account_ok {
                let mut store = OplogContentSyncStore::new(conn, account_id, time::now_ms);
                match rag_rat_sync::connect_and_reconcile(
                    &endpoint,
                    addr,
                    rag_rat_sync::CONTENT_SYNC_ALPN,
                    &mut store,
                    AuthPolicy::Closed,
                    time::now_ms,
                    rag_rat_sync::MAX_RECONCILE_ROUNDS,
                )
                .await
                {
                    Ok(report) => tracing::info!(
                        peer,
                        rounds = report.rounds,
                        converged = report.converged,
                        sent = report.entries_sent,
                        stored = report.entries_newly_stored,
                        "device sync (content) complete"
                    ),
                    Err(e) => tracing::warn!(peer, error = %e, "device sync (content) failed"),
                }
            }
            reached.push(account_ok);
        }
        anyhow::Ok(fold_peer_outcomes(resolved.unresolved_configured, &reached))
    });

    // Stamp the cadence watermark on ANY completed attempt — a per-peer failure OR an endpoint-bind
    // failure (a bad relay URL, a relay outage). Without stamping before we propagate, the bind
    // failure would `?`-exit and every subsequent hook would rebind and hit the relay again,
    // ignoring `push_interval_secs`. A bind failure still surfaces (the caller reports it and never
    // fails the hook), but it is now cadence-limited exactly like an unreachable peer.
    record_device_sync(conn)?;
    let (peers, ok, errors) = result?;
    Ok(DeviceSyncOutcome::Ran { peers, ok, errors })
}

/// The discovery service's dialable address, or `None` (logged) when the configured id is unusable.
///
/// One seam for both callers — the device-sync pass and the serving host — so the resolution rule,
/// the `[sync] discovery` switch, and the malformed-id warning cannot drift apart. `None` means
/// this invocation does not talk to the discovery service, for either reason. The service is a
/// separate iroh peer reached BY NODE ID through the same relay as the peers, not the relay
/// itself.
fn discovery_service_addr(config: &Config, relay: &str) -> Option<rag_rat_sync::EndpointAddr> {
    // The single switch. Gating HERE rather than at each caller is what makes
    // `[sync] discovery = false` mean "no contact with the service" rather than "no contact from
    // whichever paths someone remembered to check" — a new caller inherits it by construction.
    if !config.sync.discovery {
        return None;
    }
    let discovery_node = effective_discovery_node(config);
    match rag_rat_sync::peer_addr(&discovery_node, relay) {
        Ok(addr) => Some(addr),
        Err(error) => {
            tracing::warn!(
                service = discovery_node,
                %error,
                "skipping peer discovery: [sync] discovery_node_id is not a usable node id"
            );
            None
        },
    }
}

/// Seal this host's announcement to the account's current roster, or `None` when there is nothing
/// to advertise.
///
/// `None` covers a store with no account and a roster holding only this device — the latter has no
/// one to be discovered BY, so publishing would spend a slot to tell nobody. A sealing failure is
/// logged and treated the same way: discovery is routing advice, and a host that cannot advertise
/// still serves every peer that reaches it through a configured `server_peers` entry.
fn seal_announcement(
    conn: &Connection,
    tag: &[u8; 32],
    local_node: &[u8; 32],
) -> Option<(Vec<u8>, rag_rat_oplog::discovery::RosterStamp)> {
    match rag_rat_oplog::discovery::seal_discovery_announcement(conn, tag, local_node) {
        Ok(Some(sealed)) if sealed.recipients > 1 => Some((sealed.bytes, sealed.roster_stamp)),
        Ok(Some(_)) => {
            tracing::debug!("not advertising: this device is the account's only roster member");
            None
        },
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "not advertising: sealing this host's announcement failed");
            None
        },
    }
}

/// Whether a serving host should advertise itself.
///
/// Three independent reasons not to, each easy to get wrong by omission rather than by writing
/// something false: `[sync] discovery` turns the service off entirely, `[sync] discoverable` is
/// opt-in on top of that, and `--once` is a scripted single-connection check rather than a host —
/// advertising from it would publish an announcement that outlives the process by a whole TTL,
/// pointing peers at a node that has already exited.
///
/// Named and tested separately because the composition it feeds is not reachable from a test: the
/// caller binds an endpoint, takes the session lock, and runs an accept loop until interrupted.
fn serve_should_advertise(discovery: bool, discoverable: bool, once: bool) -> bool {
    discovery && discoverable && !once
}

/// A spawned task that is aborted when this guard drops, so a background loop cannot outlive the
/// resources it borrows conceptually (here: the endpoint `serve` advertises).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Fold the per-peer session results into the `(peers, ok, errors)` a [`DeviceSyncOutcome::Ran`]
/// reports. `reached[i]` is whether peer `i`'s account-log session succeeded.
///
/// Pure, and separately tested, because this accounting is where the mistake lives when it is made.
/// The obvious spelling — deriving the unresolvable count as `configured - peers.len()` — is wrong
/// the moment discovery can add peers or duplicate spellings can collapse, and it under-counts
/// errors all the way to a healthy-looking zero over an all-typo `server_peers`. A unit test on the
/// resolver's return value cannot see that, because the subtraction lives HERE, at the caller.
fn fold_peer_outcomes(unresolved_configured: usize, reached: &[bool]) -> (usize, usize, usize) {
    let ok = reached.iter().filter(|reached| **reached).count();
    let errors = reached.len() - ok + unresolved_configured;
    (reached.len() + unresolved_configured, ok, errors)
}

/// The cadence gate: whether enough time has passed since the last device-side sync attempt.
/// `push_interval_secs == 0` always runs; a never-synced store always runs.
fn device_sync_due(conn: &Connection, push_interval_secs: u64) -> anyhow::Result<bool> {
    if push_interval_secs == 0 {
        return Ok(true);
    }
    let Some(last) = rag_rat_db::meta::read_meta(conn, DEVICE_SYNC_LAST_META_KEY)?
        .and_then(|s| s.trim().parse::<i64>().ok())
    else {
        return Ok(true);
    };
    let now = time::now_ms();
    // A watermark in the FUTURE means the clock stepped backward (NTP correcting a fast RTC, a VM
    // resume) after a stamp — never trust it, or device sync would stay silently suppressed for the
    // whole gap until the wall clock catches up. Treat it as due and let the next run re-stamp.
    if last > now {
        return Ok(true);
    }
    let interval_ms = i64::try_from(push_interval_secs).unwrap_or(i64::MAX).saturating_mul(1000);
    Ok(now - last >= interval_ms)
}

/// Stamp the device-side sync cadence watermark at the current time.
fn record_device_sync(conn: &Connection) -> anyhow::Result<()> {
    rag_rat_db::meta::set_meta(conn, DEVICE_SYNC_LAST_META_KEY, &time::now_ms().to_string())
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

/// This store's local device capability when it is roster-EFFECTIVE — not merely present. Mints its
/// own binding for `local_node` and runs it through the roster `authorize` path
/// (self-authorization). Returns `None` for both "no local device" (empty binding) and "removed
/// from the roster" (a nonempty binding a `Closed` peer would reject as `NotRosterDevice`).
/// Device-side sync accepts either granted capability so a read-only device can pull; `serve`
/// requires `ReadWrite` because a read-only store-and-forward peer could accept pushes but never
/// restore data to clients.
fn device_roster_capability(
    conn: &Connection,
    account_id: rag_rat_oplog::AccountId,
    local_node: &[u8; 32],
) -> anyhow::Result<Option<PeerCapability>> {
    let probe = OplogSyncStore::new(conn, account_id, time::now_ms);
    let now = time::now_ms();
    let local = probe.local_auth(local_node, now)?;
    match probe.authorize(&local.binding, local_node, now)? {
        PeerAuthorization::Granted(capability) => Ok(Some(capability)),
        PeerAuthorization::Rejected | PeerAuthorization::Unavailable => Ok(None),
    }
}

fn device_can_serve(capability: Option<PeerCapability>) -> bool {
    matches!(capability, Some(PeerCapability::ReadWrite))
}

fn device_can_sync(capability: Option<PeerCapability>) -> bool {
    capability.is_some()
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
    use std::path::PathBuf;

    use rag_rat_base::config::Config;
    use rag_rat_base::hash;
    use rusqlite::Connection;

    use super::{
        DEVICE_SYNC_LAST_META_KEY, DeviceSyncOutcome, decode_node_secret, device_can_serve,
        device_can_sync, device_sync_due, device_sync_run, discovery_node_or_configured,
        discovery_service_addr, fold_peer_outcomes, join, node_secret, record_device_sync,
        serve_should_advertise,
    };

    fn account_of(conn: &Connection) -> rag_rat_oplog::AccountId {
        rag_rat_oplog::read_local_account(conn).unwrap().expect("the fixture minted an account")
    }

    /// How many devices the LOCAL roster projection believes are effective, counted directly so a
    /// test can prove the state it claims to exercise without a public API existing for its sake.
    fn roster_device_count(conn: &Connection, account: rag_rat_oplog::AccountId) -> usize {
        conn.query_row(
            "SELECT COUNT(DISTINCT device_fingerprint) FROM account_roster_history
             WHERE account_id = ?1 AND closed_at IS NULL",
            rusqlite::params![account.to_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap() as usize
    }

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        conn
    }

    /// A minimal config whose paths are never touched by these gate tests (they all return before
    /// the session lock / network). `[sync]` starts at its defaults (no peers).
    fn min_config() -> Config {
        Config::minimal_for_database(
            PathBuf::from("/nonexistent/db.sqlite"),
            PathBuf::from("/nonexistent"),
        )
    }

    /// `min_config` with a real database path, for the tests that run far enough into
    /// `device_sync_run` to take the per-database session lock — which needs a directory it can
    /// actually create lock files in. The returned `TempDir` must outlive the config.
    fn config_with_real_lock_dir() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config =
            Config::minimal_for_database(dir.path().join("db.sqlite"), dir.path().to_path_buf());
        (config, dir)
    }

    #[test]
    fn device_sync_due_respects_the_cadence() {
        let conn = schema_conn();
        assert!(device_sync_due(&conn, 0).unwrap(), "interval 0 always runs");
        assert!(device_sync_due(&conn, 300).unwrap(), "a never-synced store is due");
        record_device_sync(&conn).unwrap();
        assert!(
            !device_sync_due(&conn, 300).unwrap(),
            "a just-synced store waits out the interval"
        );
        rag_rat_db::meta::set_meta(&conn, DEVICE_SYNC_LAST_META_KEY, "0").unwrap();
        assert!(device_sync_due(&conn, 300).unwrap(), "an ancient watermark is due again");
        // A watermark in the future (backward clock step) must not silently suppress sync.
        let future = (rag_rat_base::time::now_ms() + 10_000_000).to_string();
        rag_rat_db::meta::set_meta(&conn, DEVICE_SYNC_LAST_META_KEY, &future).unwrap();
        assert!(device_sync_due(&conn, 300).unwrap(), "a future watermark is treated as due");
    }

    /// The accounting the old `configured.saturating_sub(peers.len())` got wrong. Restoring that
    /// subtraction makes the discovery row report 1 error instead of 3, and the all-typo row report
    /// 0 instead of 2 — a pass that looks healthy while syncing with nobody.
    #[test]
    fn peer_outcomes_count_every_attempt_including_the_ids_that_never_resolved() {
        assert_eq!(
            fold_peer_outcomes(0, &[true, true]),
            (2, 2, 0),
            "two configured peers, both reached"
        );
        assert_eq!(
            fold_peer_outcomes(2, &[]),
            (2, 0, 2),
            "an all-typo server_peers reports errors, never a healthy-looking empty run"
        );
        assert_eq!(
            fold_peer_outcomes(3, &[true, false]),
            (5, 1, 4),
            "unresolvable ids are attempts too: they seed errors and count toward peers"
        );
        // Discovery adds peers no config named, so `peers` exceeds the configured count. The old
        // subtraction could not represent this at all.
        let (peers, ok, errors) = fold_peer_outcomes(1, &[true, true, false, true]);
        assert_eq!((peers, ok, errors), (5, 3, 2));
        assert_eq!(
            ok + errors,
            peers,
            "ok + errors == peers is the invariant the hook report reads"
        );
    }

    /// The env override exists for ops and tests; without it, pointing a checkout at a throwaway
    /// discovery service would mean editing committed config.
    #[test]
    fn the_discovery_node_env_override_wins_over_the_configured_service() {
        let mut config = min_config();
        config.sync.discovery_node_id = "configured-node".to_string();
        assert_eq!(
            discovery_node_or_configured(Some("  env-node  "), &config),
            "env-node",
            "the override wins and is trimmed"
        );
        for absent in [None, Some(""), Some("   ")] {
            assert_eq!(
                discovery_node_or_configured(absent, &config),
                "configured-node",
                "an unset or blank override falls through to the configured service ({absent:?})"
            );
        }
    }

    /// Both reasons a serving host stays silent, enumerated — each is an omission-shaped mistake.
    ///
    /// `--once` matters more than it looks: an announcement outlives the process by a whole TTL, so
    /// a one-shot check that advertised would leave peers dialing a node that has already exited.
    #[test]
    fn a_serving_host_advertises_only_when_opted_in_and_not_running_one_shot() {
        assert!(
            serve_should_advertise(true, true, false),
            "a discoverable long-running host advertises"
        );
        assert!(!serve_should_advertise(true, false, false), "[sync] discoverable is opt-in");
        assert!(
            !serve_should_advertise(false, true, false),
            "[sync] discovery = false means there is no service to advertise to, whatever else \
             says"
        );
        assert!(
            !serve_should_advertise(true, true, true),
            "--once is a scripted check, not a host; its announcement would outlive the process"
        );
        assert!(!serve_should_advertise(false, false, true));
    }

    /// `[sync] discovery = false` silences the service for BOTH callers, because both resolve its
    /// address through one seam. Gating at each call site instead would leave a new caller talking
    /// to the service by default.
    #[test]
    fn switching_discovery_off_leaves_no_service_address_for_anyone_to_use() {
        let (mut config, _dir) = config_with_real_lock_dir();
        assert!(
            discovery_service_addr(&config, "https://relay.invalid").is_some(),
            "the shipped default resolves, or the negative below proves nothing"
        );

        config.sync.discovery = false;
        assert!(
            discovery_service_addr(&config, "https://relay.invalid").is_none(),
            "no address means no fetch and nothing to advertise to"
        );
        assert!(
            !serve_should_advertise(config.sync.discovery, true, false),
            "and a host cannot advertise even with discoverable set"
        );
    }

    /// A pass with NO configured peers and a roster that shows only this device still runs.
    ///
    /// It used to return `Disabled` here, on the reasoning that a lone device has nobody to find.
    /// That reasoning was circular: the roster count is replicated state, current only if this
    /// device has synced recently, so a store restored from a backup predating the other devices
    /// believes it is alone, declines to look, and never receives the entries that would correct
    /// it — wedged forever while a reachable host advertises. Re-introduce that gate and this test
    /// goes red.
    ///
    /// Hermetic: a reserved-TLD relay and an unparseable service id mean no network call. That is
    /// also this test's limit, and it is worth naming rather than implying: because the service id
    /// does not resolve, the pass "looks" at nothing, so the test pins the ABSENCE of the early
    /// return and not the presence of a discovery query. **One mutation survives it, and the whole
    /// suite: passing `None` as `discover_peers`' discovery argument whenever `server_peers` is
    /// empty.** That reintroduces exactly the wedge described above. Closing it needs the driver to
    /// reach an in-process service, which needs either a live relay (the repository already gates
    /// such tests behind `RAG_RAT_SYNC_RELAY`) or a test-only injection point in production code —
    /// neither of which is worth more than saying so here.
    #[test]
    fn a_lone_looking_roster_does_not_stop_the_pass_from_looking() {
        let conn = schema_conn();
        rag_rat_oplog::local_account(&conn, 1_700_000_000_000).unwrap();
        assert_eq!(
            roster_device_count(&conn, account_of(&conn)),
            1,
            "exactly the state that used to short-circuit: the roster knows of nobody else"
        );

        let (mut config, _dir) = config_with_real_lock_dir();
        assert!(config.sync.server_peers.is_empty(), "and nothing is configured either");
        config.sync.relay_url = "https://relay.invalid".to_string();
        config.sync.discovery_node_id = "not-a-node-id".to_string();

        assert_eq!(
            device_sync_run(&config, &conn).unwrap(),
            DeviceSyncOutcome::Ran { peers: 0, ok: 0, errors: 0 },
            "the pass runs and looks; zero peers were reachable, which is not an error"
        );
    }

    #[test]
    fn device_sync_run_is_disabled_without_a_local_account() {
        let conn = schema_conn(); // schema only, never minted an account
        let mut config = min_config();
        config.sync.server_peers = vec!["some-node-id".to_string()];
        assert_eq!(
            device_sync_run(&config, &conn).unwrap(),
            DeviceSyncOutcome::Disabled,
            "configured peers but no enrolled account => nothing to sync (and never mints one)"
        );
    }

    #[test]
    fn device_sync_run_skips_within_the_cadence_window() {
        let conn = schema_conn();
        rag_rat_oplog::local_account(&conn, 1_700_000_000_000).unwrap(); // enroll a local account
        record_device_sync(&conn).unwrap(); // just synced
        let mut config = min_config();
        config.sync.server_peers = vec!["some-node-id".to_string()];
        config.sync.push_interval_secs = 300;
        assert_eq!(
            device_sync_run(&config, &conn).unwrap(),
            DeviceSyncOutcome::Skipped,
            "a recent sync suppresses the next attempt before any dial"
        );
    }

    #[test]
    fn read_only_devices_can_sync_but_cannot_serve() {
        let read_only = Some(rag_rat_sync::PeerCapability::ReadOnly);
        assert!(device_can_sync(read_only));
        assert!(!device_can_serve(read_only));
        assert!(device_can_serve(Some(rag_rat_sync::PeerCapability::ReadWrite)));
        assert!(!device_can_sync(None));
    }

    #[test]
    fn join_rejects_a_malformed_ticket_before_touching_the_store() {
        // The ticket is decoded before any lock or index open, so a bad ticket fails fast without a
        // real database — `min_config` points at a nonexistent path that must not be opened here.
        let err = join(&min_config(), "not-a-real-ticket").unwrap_err();
        assert!(
            err.to_string().contains("invalid enrollment ticket"),
            "a malformed ticket is rejected up front: {err}",
        );
    }

    #[test]
    fn invite_role_maps_to_the_device_role() {
        use rag_rat_oplog::DeviceRole;

        use crate::cli::InviteRole;
        assert_eq!(InviteRole::ReadOnly.to_device_role(), DeviceRole::ReadOnly);
        assert_eq!(InviteRole::Member.to_device_role(), DeviceRole::Member);
        assert_eq!(InviteRole::Owner.to_device_role(), DeviceRole::Owner);
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
