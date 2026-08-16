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
        SyncCommand::Pull { account, peer } => return pull(config, account, peer.as_deref()),
        SyncCommand::Enable
        | SyncCommand::Publish { .. }
        | SyncCommand::CatchUp { .. }
        | SyncCommand::Whoami
        | SyncCommand::Grant { .. }
        | SyncCommand::Contribute { .. } => {},
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
        SyncCommand::Publish { seed } => {
            let (published, seeded_memories) = match seed.as_deref() {
                Some(path) => {
                    let report = db.sync_publish_seed(path)?;
                    (report.published, Some(report.imported_memories))
                },
                None => (db.sync_publish()?, None),
            };
            print_output(&serde_json::json!({
                "status": if published { "published" } else { "already_published" },
                "repo_id": db.active_repo_id,
                "public_read": true,
                "seeded_memories": seeded_memories,
                "transport_configured": false,
                "note": "this account is now a public knowledge base; subsequent memory changes are public and the account is servable to anonymous readers once `sync serve` runs",
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
        SyncCommand::Whoami => {
            let account_id = db.sync_whoami()?;
            print_output(&serde_json::json!({
                "account_id": account_id,
                "repo_id": db.active_repo_id,
                "note": "share this account id with an owner so they can `sync grant` it write access to their repo's memories",
            }))
        },
        SyncCommand::Grant { account } => {
            let grant_id = db.sync_grant(account)?;
            print_output(&serde_json::json!({
                "status": "granted",
                "repo_id": db.active_repo_id,
                "grantee_account_id": account,
                "grant_id": grant_id,
                "role": "writer",
                "note": "the grantee may now author memories into this repo once it syncs this account's log; revoke is a separate command (not yet available)",
            }))
        },
        SyncCommand::Contribute { account } => {
            db.sync_contribute(account)?;
            print_output(&serde_json::json!({
                "status": "contributing",
                "repo_id": db.active_repo_id,
                "owner_account_id": account,
                "note": "memory changes for this repo now target the owner's stream; the owner must `sync grant` this account and this store must sync the owner's log before authoring succeeds",
            }))
        },
        SyncCommand::Serve { .. }
        | SyncCommand::Init { .. }
        | SyncCommand::Join { .. }
        | SyncCommand::Pull { .. } =>
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

/// A one-time enrollment invite `sync init` mints as it starts hosting the pairing exchange.
struct InviteMint {
    role: rag_rat_oplog::DeviceRole,
    label: Option<String>,
    ttl: Duration,
}

/// Run a headless store-and-forward peer for this account's op log: bind the sync endpoint over the
/// configured relay and replicate with peers the roster authorizes. Serves every stream a peer may
/// negotiate — the account log (`SYNC_ALPN`), `/3` content (`CONTENT_SYNC_ALPN`), and repo-scoped
/// `/5` tables (`TABLE_SYNC_ALPN`) — routing
/// each connection by its ALPN. Runs until interrupted; `once` serves a single connection (one
/// stream), so a full sync requires one connection per supported ALPN.
fn serve(config: &Config, once: bool) -> anyhow::Result<()> {
    serve_with(config, once, None)
}

/// Owner-side pairing (`sync init`): mint a one-time invite, print the ticket, then host the
/// enrollment exchange AND the joiner's follow-up account + content restore until interrupted. It
/// is the same accept loop as [`serve`] — `dispatch_connection` already routes the enrollment ALPN
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
    ensure_founder_table_repo_incarnations(db.connection())?;
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

        // A host is advertised by the same persisted controller the resident MCP host uses. It
        // owns its timer and DB handles, so resealing never waits for (or cancels) an inbound sync.
        let _advertiser = (!once).then(|| {
            AbortOnDrop(tokio::spawn(rag_rat_core::sync_driver::advertise_host(
                config.clone(),
                endpoint.clone(),
                config.database.clone(),
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
        // `local_node` is already bound above (the roster gate); reuse it for
        // `dispatch_connection`. Global inbound accept-rate limit: refuses a connection
        // flood BEFORE the handshake, regardless of peer id (Sybil-resistant). Loop-owned;
        // the accept is sequential so no locking.
        let mut accept_rate = rag_rat_sync::GlobalAcceptRateLimiter::new();
        // Global egress byte cap bounding total data served — a peer cannot drain the host by
        // re-pulling. An `Arc<Mutex>` to match the shared-handle API (the CLI accept is sequential,
        // so there is never real contention).
        let egress =
            std::sync::Arc::new(std::sync::Mutex::new(rag_rat_sync::GlobalEgressLimiter::new()));
        loop {
            // One endpoint, one accept loop: a connection carries the ACCOUNT-LOG ALPN or the
            // CONTENT ALPN (or ENROLL/TABLE), and `dispatch_connection` routes it to
            // the matching store after the (account-level) auth phase. Both stores are
            // cheap handles reconstructed per accept.
            let mut account_store = OplogSyncStore::new(conn, account_id, time::now_ms);
            let mut content_store = OplogContentSyncStore::new(conn, account_id, time::now_ms);
            // Accept + rate check + dispatch stay in ONE selected future so ctrl_c interrupts a
            // peer mid-session. `Some(None)` = refused by the accept-rate limit
            // (nothing served); `None` = shutdown. `accept_connection_within_rate`
            // reads `now_ms` once a peer connects, so the auth timestamp never predates
            // the connection.
            let outcome = tokio::select! {
                result = async {
                    match rag_rat_sync::accept_connection_within_rate(
                        &endpoint,
                        &mut accept_rate,
                        time::now_ms,
                    )
                    .await
                    {
                        Ok(Some(incoming)) => {
                            // Serve a published public-KB account PublicRead (anonymous read),
                            // everything else Closed — derived AFTER the peer lands so a `sync
                            // publish` while parked waiting takes effect on this very connection. A
                            // DB read fault fails closed (Closed); the store's fully-public snapshot
                            // guard is the backstop either way.
                            let policy = if rag_rat_core::sync_driver::account_is_public_kb(
                                conn, account_id,
                            )
                            .unwrap_or(false)
                            {
                                AuthPolicy::PublicRead
                            } else {
                                AuthPolicy::Closed
                            };
                            Some(
                                rag_rat_sync::dispatch_connection(
                                    incoming,
                                    local_node,
                                    &mut account_store,
                                    &mut content_store,
                                    policy,
                                    time::now_ms,
                                    Some(egress.clone()),
                                )
                                .await,
                            )
                        },
                        Ok(None) => None,
                        Err(error) => Some(Err(error)),
                    }
                } => Some(result),
                _ = &mut shutdown => None,
            };
            match outcome {
                None => {
                    tracing::info!("interrupted; shutting down");
                    break;
                },
                // Refused by the accept-rate limit before the handshake — nothing served, take the
                // next connection (never an error, so `--once` is unaffected by a refusal).
                Some(None) => continue,
                Some(Some(Ok((alpn, report)))) => {
                    if alpn.as_slice() == rag_rat_sync::SYNC_ALPN
                        && report.entries_sent == 0
                        && report.entries_received == 0
                        && report.entries_newly_stored == 0
                    {
                        ensure_founder_table_repo_incarnations(conn)?;
                    } else if alpn.as_slice() == rag_rat_sync::CONTENT_SYNC_ALPN {
                        rag_rat_core::drain_synced_memory(conn)?;
                    } else if alpn.as_slice() == rag_rat_sync::TABLE_SYNC_ALPN {
                        // Synced anchors arrive without device-local resolution; derive it now so
                        // they surface as drive-by this session rather than at the next index open.
                        rag_rat_core::resolve_synced_distill_anchors(conn)?;
                    }
                    tracing::info!(
                        stream = %String::from_utf8_lossy(&alpn),
                        sent = report.entries_sent,
                        received = report.entries_received,
                        stored = report.entries_newly_stored,
                        "sync session complete"
                    );
                },
                // In one-shot mode the session IS the command's result, so a failed session is a
                // failed command (scripted checks must see the non-zero exit). The long-running
                // server logs and moves on to the next peer instead.
                Some(Some(Err(e))) if once => return Err(anyhow!("sync session failed: {e}")),
                Some(Some(Err(e))) => {
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

        // 2) Restore-from-zero — pull the account log, `/3` content, then supported `/5` table
        //    streams from the inviter, now that this device is roster-effective. Content rides on
        //    the account log's authority, so the account log runs first. Each stream reconciles to
        //    a fixpoint (#878): a single session can report `Done` while the store is still
        //    incomplete, so re-run until dry (or the round cap). The inviter must still be serving
        //    (its `sync init` / `serve`).
        let mut account_report = rag_rat_sync::ReconcileReport {
            rounds: 0,
            entries_newly_stored: 0,
            entries_sent: 0,
            converged: false,
            peer_capability: PeerCapability::ReadOnly,
        };
        for pass in 0..3 {
            let mut store = OplogSyncStore::new(conn, account_id, time::now_ms);
            let report = rag_rat_sync::connect_and_reconcile(
                &endpoint,
                peer.clone(),
                rag_rat_sync::SYNC_ALPN,
                &mut store,
                AuthPolicy::Closed,
                time::now_ms,
                rag_rat_sync::MAX_RECONCILE_ROUNDS,
            )
            .await
            .map_err(|e| anyhow!("restoring the account log from the inviter failed: {e}"))?;
            if !report.converged {
                bail!("restoring the account log did not converge before the round limit");
            }
            accumulate_reconcile_report(&mut account_report, report);
            if pass == 1 {
                ensure_founder_table_repo_incarnations(conn)?;
            }
        }
        let content_report = {
            let mut store = OplogContentSyncStore::new(conn, account_id, time::now_ms);
            rag_rat_sync::connect_and_reconcile(
                &endpoint,
                peer.clone(),
                rag_rat_sync::CONTENT_SYNC_ALPN,
                &mut store,
                AuthPolicy::Closed,
                time::now_ms,
                rag_rat_sync::MAX_RECONCILE_ROUNDS,
            )
            .await
            .map_err(|e| anyhow!("restoring content from the inviter failed: {e}"))?
        };
        rag_rat_core::drain_synced_memory(conn)?;
        let tables_converged = {
            let mut store = rag_rat_sync::OplogTableSyncStore::new(conn, account_id, time::now_ms);
            if store.has_streams()? {
                rag_rat_sync::connect_and_table_reconcile(
                    &endpoint,
                    peer,
                    &mut store,
                    time::now_ms,
                    rag_rat_sync::MAX_RECONCILE_ROUNDS,
                )
                .await
                .map_err(|e| anyhow!("restoring table streams from the inviter failed: {e}"))?
                .converged
            } else {
                true
            }
        };
        // Resolve the anchors this restore just pulled against the local index before folding.
        rag_rat_core::resolve_synced_distill_anchors(conn)?;
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
            "converged": account_report.converged
                && content_report.converged
                && tables_converged,
            "inviter_node_id": inviter,
            "inviter_relay": ticket.relay_url,
            "note": "to keep syncing, add inviter_node_id to [sync] server_peers and set [sync] \
                     relay_url (or RAG_RAT_SYNC_RELAY) to inviter_relay",
        }))?;
        anyhow::Ok(())
    })
}

/// Fetch a DIFFERENT account's log and content from a peer, then materialize them locally.
///
/// The escape hatch behind automatic sync (#1174): the resident host runs this same shape after a
/// HEAD change, and an operator reaches for the command when automation is off. Cross-account
/// contribution needs it in both directions — a contributor fetches the owner's memories, and an
/// owner collects a contributor's — because content is offered by AUTHOR
/// (`content_entries_for_sync` filters `author_account_id`), so each side must sync the OTHER's
/// account to see what that side wrote.
///
/// Deliberately NOT a `sync join`: no enrollment, no `/5` table restore (foreign table streams are
/// private account data, pinned `Closed`), and no founder-incarnation repair.
fn pull(config: &Config, account_hex: &str, peer_override: Option<&str>) -> anyhow::Result<()> {
    let target = rag_rat_oplog::AccountId::from_hex(account_hex)?;
    let relay = effective_relay_url(config);

    // The per-database SESSION lock, held for the whole pull. Any process that opens an iroh
    // endpoint for a database must hold it: the endpoint identity comes from the persisted
    // `sync_node_secret`, so a second endpoint binds the SAME node id and the two race relay
    // registration and inbound sessions. A resident MCP host or `sync serve` holds this for its
    // lifetime, which is exactly the common case here — an operator reaching for `pull` while the
    // resident is up.
    let _session = locks::WriteLock::acquire_sync_session_timeout(
        &config.database,
        SERVE_SESSION_LOCK_TIMEOUT,
    )?
    .ok_or_else(|| {
        anyhow!(
            "another sync session already holds this database's node identity (a resident MCP \
             host, a `serve` peer, or a device sync is running); stop it and retry — it cannot \
             pull a foreign account on your behalf"
        )
    })?;

    let lock_repo = locks::write_lock_repo_id(config);
    let repo_lock = locks::WriteLock::acquire_timeout(
        &config.database,
        &lock_repo,
        SERVE_SESSION_LOCK_TIMEOUT,
    )?
    .ok_or_else(|| anyhow!("the index write lock is busy (another writer is mid-pass); retry"))?;
    let db = crate::open_index(config)?;
    let node_key = {
        let conn = db.connection();
        // Pulling your OWN account is device sync, not a cross-account fetch — say so rather than
        // opening a session that would work but confuse the mental model.
        if rag_rat_oplog::read_local_account(conn)? == Some(target) {
            bail!(
                "that is this store's own account — use `rag-rat sync serve` on the other device \
                 and let device sync run, or `sync join` to enroll a new one"
            );
        }
        rag_rat_oplog::local_device(conn, time::now_ms())?;
        node_secret(conn)?
    };
    drop(repo_lock);

    let peers: Vec<String> = match peer_override {
        Some(peer) => vec![peer.to_string()],
        None => config.sync.server_peers.clone(),
    };
    if peers.is_empty() {
        bail!(
            "no peer to pull from: pass --peer <NODE_ID> or set [sync] server_peers. Discovery \
             cannot find a FOREIGN account's host — its discovery tag derives from that account's \
             own secret, which only its own devices hold"
        );
    }

    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async move {
        let conn = db.connection();
        let endpoint = rag_rat_sync::build_endpoint(*node_key, &relay)
            .await
            .with_context(|| format!("binding the sync endpoint over relay {relay}"))?;

        let mut last_error: Option<String> = None;
        // Durable across attempts: a peer can store entries and then fail to converge, and those
        // bytes stay. Reporting only the final peer's tally would undercount — sometimes to zero.
        let mut account_entries = 0usize;
        let mut content_entries = 0usize;
        let mut reached: Option<(String, bool)> = None;
        for peer_id in &peers {
            let addr = match rag_rat_sync::peer_addr(peer_id, &relay) {
                Ok(addr) => addr,
                Err(error) => {
                    last_error = Some(format!("peer `{peer_id}` is not a valid node id: {error}"));
                    continue;
                },
            };
            // ACCOUNT LOG FIRST, then content: content acceptance re-derives authority from the
            // account log, so a content session run first would park every candidate until a later
            // settle. One command, correct order, nothing parked in the normal case.
            //
            // `PublicRead`, never `Closed`: on first contact this store holds ZERO roster facts for
            // the foreign account, so `authorize` returns `Unavailable` — which `Closed` maps to
            // `Unauthorized`, failing every first pull. `PublicRead` maps `Unavailable` + dialer to
            // the ReadWrite bootstrap fallback built for exactly this. Admission is not trust:
            // `account_ingest` / `content_ingest` re-verify every entry from scratch.
            let mut account_store = OplogSyncStore::new(conn, target, time::now_ms);
            let account_report = match rag_rat_sync::connect_and_reconcile(
                &endpoint,
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
                    last_error = Some(format!("{peer_id}: account log: {error}"));
                    continue;
                },
            };
            // A non-converged account leg means the round cap was hit with the store still possibly
            // incomplete. Content acceptance re-derives authority from that log, so proceeding
            // would silently leave valid entries unaccepted — and a cross-account pull has no
            // automatic retry to repair it later. Treat the peer as unusable and try the next.
            account_entries += account_report.entries_newly_stored;
            // A pull exists to RECEIVE. If this side granted the peer only `ReadOnly`, its entries
            // are rejected on arrival, so an all-quiet round means "structurally unable to receive"
            // rather than "in sync" — and `converged` would report success on an incomplete
            // account. This is the resumed-bootstrap wedge: once a partial pull leaves
            // `account_effective_count > 0` for the target, a serving device whose `DeviceAdd` has
            // not arrived folds `Rejected` (not `Unavailable`), which loses the bootstrap fallback.
            if account_report.peer_capability != PeerCapability::ReadWrite {
                last_error = Some(format!(
                    "{peer_id}: this store holds a PARTIAL roster for that account, so it could \
                     not authorize this peer to serve — the peer was admitted read-only and sent \
                     nothing. Pull from the peer whose device is already in the roster you hold \
                     (usually the account's own host), or start from a store with no entries for \
                     it"
                ));
                continue;
            }
            if !account_report.converged {
                last_error = Some(format!(
                    "{peer_id}: the account log did not converge before the round limit; its \
                     content would be judged against incomplete authority"
                ));
                continue;
            }
            let mut content_store = OplogContentSyncStore::new(conn, target, time::now_ms);
            let content_report = match rag_rat_sync::connect_and_reconcile(
                &endpoint,
                addr,
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
                    last_error = Some(format!("{peer_id}: content: {error}"));
                    continue;
                },
            };
            content_entries += content_report.entries_newly_stored;
            if !content_report.converged {
                // Same treatment as the account leg: a healthy later peer may finish the job, and
                // breaking here would pin every re-run on the same non-converging first peer. The
                // entries this peer did store are durable and stay counted.
                last_error =
                    Some(format!("{peer_id}: content did not converge before the round limit"));
                continue;
            }
            reached = Some((peer_id.clone(), true));
            break;
        }

        let Some((peer_id, converged)) = reached else {
            bail!(
                "could not pull account {} from any configured peer: {}",
                hash::hex_lower(&target.to_bytes()),
                last_error.unwrap_or_else(|| "no peer reachable".to_string())
            );
        };

        // Materialize what landed, and MEASURE the result rather than asserting it. The drain
        // mirrors exactly ONE authoritative stream per repo: the configured contribution owner's,
        // or this store's own. Those cover the two directions contribution needs — a contributor
        // pulling its owner, and an owner pulling a contributor (whose entries sit on the owner's
        // own stream). A standalone reader pulling some third account it neither owns nor
        // contributes to has nowhere to put the content, and must not be told otherwise.
        let effects = rag_rat_core::drain_synced_memory(conn)?;
        rag_rat_core::resolve_synced_distill_anchors(conn)?;
        db.fold_wal();

        let note = if !converged {
            "the round limit was reached before this account converged — re-run to finish; what \
             arrived is durable"
        } else if effects.nodes_written > 0 || effects.edges_written > 0 {
            "these memories are searchable locally; their code anchors do not cross an account \
             boundary, so they will not attach as drive-by context"
        } else if !effects.is_empty() {
            "the authority this pull brought RETRACTED memories here — the drain removed what the \
             owner's log no longer accepts; nothing new was added"
        } else if content_entries > 0 {
            "content arrived but nothing materialized into this repo's memories: a repo mirrors \
             ONE stream — its own, or a configured contribution owner's. Run `sync contribute \
             <this account>` to mirror it, or pull from a store that owns or contributes to it"
        } else {
            "already up to date with this account"
        };
        crate::print_output(&serde_json::json!({
            "status": if converged { "pulled" } else { "incomplete" },
            "account_id": hash::hex_lower(&target.to_bytes()),
            "peer": peer_id,
            "account_entries_stored": account_entries,
            "content_entries_stored": content_entries,
            "memories_written": effects.nodes_written,
            "memories_removed": effects.nodes_removed,
            "edges_written": effects.edges_written,
            "edges_removed": effects.edges_removed,
            "converged": converged,
            "note": note,
        }))?;
        anyhow::Ok(())
    })
}

pub(crate) use rag_rat_core::sync_driver::{DeviceSyncOutcome, device_sync_run};

// Kept for the CLI's focused cadence tests; production device syncing lives in core.
fn ensure_founder_table_repo_incarnations(conn: &Connection) -> anyhow::Result<()> {
    for repo_id in rag_rat_db::schema::real_repo_ids(conn)? {
        rag_rat_oplog::ensure_repo_incarnation(conn, &repo_id, time::now_ms())?;
    }
    Ok(())
}

fn accumulate_reconcile_report(
    total: &mut rag_rat_sync::ReconcileReport,
    report: rag_rat_sync::ReconcileReport,
) {
    total.rounds += report.rounds;
    total.entries_newly_stored += report.entries_newly_stored;
    total.entries_sent += report.entries_sent;
    total.converged = report.converged;
}

/// A spawned task that is aborted when this guard drops, so a background loop cannot outlive the
/// resources it borrows conceptually (here: the endpoint `serve` advertises).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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
        DeviceSyncOutcome, accumulate_reconcile_report, decode_node_secret, device_can_serve,
        device_sync_run, ensure_founder_table_repo_incarnations, join, node_secret,
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
        rag_rat_db::meta::set_meta(
            &conn,
            "sync_device_last_at_ms",
            &rag_rat_base::time::now_ms().to_string(),
        )
        .unwrap();
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
    fn founder_bootstrap_creates_each_registered_repo_incarnation_once() {
        let conn = schema_conn();
        rag_rat_oplog::local_account(&conn, 1_700_000_000_000).unwrap();
        ensure_founder_table_repo_incarnations(&conn).unwrap();

        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-a', 'repo-a', 0)",
            [],
        )
        .unwrap();
        ensure_founder_table_repo_incarnations(&conn).unwrap();
        let account = account_of(&conn);
        let first = rag_rat_oplog::repo_incarnation_state(&conn, account, "repo-a").unwrap();

        ensure_founder_table_repo_incarnations(&conn).unwrap();
        assert_eq!(
            rag_rat_oplog::repo_incarnation_state(&conn, account, "repo-a").unwrap(),
            first,
            "a current repository incarnation is never advanced by bootstrap"
        );
    }

    #[test]
    fn reconcile_report_aggregation_preserves_all_counts_and_latest_convergence() {
        let mut total = rag_rat_sync::ReconcileReport {
            rounds: 2,
            entries_newly_stored: 3,
            entries_sent: 5,
            converged: false,
            peer_capability: rag_rat_sync::PeerCapability::ReadWrite,
        };
        accumulate_reconcile_report(&mut total, rag_rat_sync::ReconcileReport {
            rounds: 7,
            entries_newly_stored: 11,
            entries_sent: 13,
            converged: true,
            peer_capability: rag_rat_sync::PeerCapability::ReadWrite,
        });
        assert_eq!(total, rag_rat_sync::ReconcileReport {
            rounds: 9,
            entries_newly_stored: 14,
            entries_sent: 18,
            converged: true,
            peer_capability: rag_rat_sync::PeerCapability::ReadWrite,
        });
    }

    #[test]
    fn read_only_devices_can_sync_but_cannot_serve() {
        let read_only = Some(rag_rat_sync::PeerCapability::ReadOnly);
        assert!(!device_can_serve(read_only));
        assert!(device_can_serve(Some(rag_rat_sync::PeerCapability::ReadWrite)));
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
