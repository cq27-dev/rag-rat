use rag_rat_base::config::Config;
use rmcp::ServiceExt;
#[cfg(not(unix))]
use rmcp::transport::stdio;

use super::RagRatService;

/// Serve the stdio MCP server for a RESOLVED repo config — the ACTIVE server: keeps the index fresh
/// (watcher), serves the grep-hook listener, and (Unix) arms the SIGUSR1 hot-upgrade. Published
/// entry point; its `run_stdio(Config, …)` signature is preserved for source compatibility. The
/// config-less DORMANT server is [`run_stdio_dormant`].
pub async fn run_stdio(
    config: Config,
    output_format: rag_rat_core::OutputFormat,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        run_stdio_unix(config, output_format).await
    }
    #[cfg(not(unix))]
    {
        // Keep the index fresh while a session is connected; dropping the watcher on shutdown
        // runs a final timeout-skip pass. (Hot-upgrade is Unix-only.)
        let _watcher = rag_rat_core::watch::Watcher::spawn(config.clone());
        let _sync_host = start_sync_host(config.clone());
        let service = RagRatService::new(config.clone(), output_format).serve(stdio()).await?;
        let _lens_server = crate::lens_server::spawn(config);
        service.waiting().await?;
        Ok(())
    }
}

/// The DORMANT server (launched outside any rag-rat repo, so no config resolved): no watcher, no
/// hook listener, no hot-upgrade — nothing to keep fresh. It just serves the MCP protocol until the
/// client disconnects; `tools/list` still advertises the full catalog, but every `tools/call`
/// returns [`RagRatService::dormant_tool_result`]. Split from [`run_stdio`] so that published
/// `run_stdio(Config)` signature stays source-compatible, while a globally-registered `rag-rat mcp`
/// can still stay alive in a non-rag-rat project instead of dying (#603).
pub async fn run_stdio_dormant(output_format: rag_rat_core::OutputFormat) -> anyhow::Result<()> {
    // A dormant server never hot-upgrades — with no config there is no handoff directory, and no
    // index state worth carrying across an `exec`. It is still a `rag-rat mcp` process, though, so
    // a globally-registered launcher hands it `RAG_RAT_UPGRADE_BIN` like any other, which is
    // exactly what makes the fleet trigger consider it a signal target. Observe SIGUSR1 so the
    // trigger cannot terminate it; it keeps serving the dormant notice until the client
    // disconnects, and the next launch picks up the new binary.
    #[cfg(unix)]
    if let Some(mut sigusr1) =
        crate::upgrade::install_path().and_then(|_| crate::upgrade::arm_sigusr1())
    {
        // Detached: the drain's useful lifetime IS the process's, so there is nothing to abort.
        tokio::spawn(async move { while sigusr1.recv().await.is_some() {} });
    }
    let running = RagRatService::new_dormant(output_format).serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// Aborts the Unix hook-listener task on shutdown. A hot-`exec` replaces the process image instead,
/// and the successor re-elects after close-on-exec releases the descriptors.
#[cfg(unix)]
struct AbortOnDrop(tokio::task::JoinHandle<()>);

#[cfg(unix)]
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Unix `run_stdio`: serves over a [`crate::upgrade::GatedStdin`] so a `SIGUSR1` can hot-`exec`
/// the newly installed binary in place, and resumes (skipping `initialize`) when handed off to.
#[cfg(unix)]
async fn run_stdio_unix(
    config: Config,
    output_format: rag_rat_core::OutputFormat,
) -> anyhow::Result<()> {
    use std::sync::Arc;

    use rmcp::service::serve_directly;

    use crate::upgrade::{self, GatedStdin, Upgrade, UpgradeGate};

    let gate = UpgradeGate::new();
    let service = RagRatService::new(config.clone(), output_format);
    let inflight = service.inflight();

    // Observe SIGUSR1 before serving, not after — see [`upgrade::arm_sigusr1`]. Only when an
    // install target is configured, since that env var is precisely what makes this process a
    // fleet target.
    let install_path = upgrade::install_path();
    let sigusr1 = install_path.as_ref().and_then(|_| upgrade::arm_sigusr1());

    let transport = (GatedStdin::new(tokio::io::stdin(), Arc::clone(&gate)), tokio::io::stdout());

    // Resume (skip `initialize`) iff a predecessor handed off a session we can still honor.
    let running = match upgrade::take_handoff() {
        Some(handoff) if upgrade::protocol_supported(&handoff.negotiated_protocol_version) =>
            serve_directly(service, transport, Some(handoff.peer_info)),
        Some(handoff) => {
            // The new binary can't honor the negotiated protocol — exit cleanly so the client
            // reconnects and renegotiates rather than resuming on a mismatch.
            eprintln!(
                "hot-upgrade: protocol {} unsupported by this binary; exiting for clean reconnect",
                handoff.negotiated_protocol_version
            );
            return Ok(());
        },
        None => service.serve(transport).await?,
    };

    // Keep the index fresh while connected. Its lock fds are CLOEXEC, so a hot-`exec` releases
    // them automatically; on normal EOF the drop runs a final timeout-skip pass. When hot-upgrade
    // is armed, the elected watcher also watches the binary dir to drive the fleet trigger.
    let _watcher =
        rag_rat_core::watch::Watcher::spawn_with_fleet(config.clone(), install_path.clone());

    let _sync_host = start_sync_host(config.clone());

    // Grep-augment hook listener: one elected listener per worktree. Spawned after the `running`
    // match so it covers both cold start and hot-upgrade resume. On normal EOF shutdown the guard
    // aborts the task so the socket + election lock release promptly; on a hot-`exec` the process
    // image is replaced (task vanishes) and the new process re-elects.
    let _hook_listener = AbortOnDrop(crate::agent_hook::spawn_listener(config.clone()));

    // The loopback editor API belongs to the same ACTIVE lifecycle as the watcher and hook
    // listener. The task fails open: election, bind, or publication errors are warnings and never
    // terminate the stdio MCP service.
    let _lens_server = crate::lens_server::spawn(config.clone());

    // The signal is already observed; now that the session is understood, decide what it DOES.
    //
    // A peer that used the stateless lifecycle — protocol 2026-07-28 drops `initialize` and carries
    // its version + capabilities in each request's `_meta` — leaves `peer_info()` empty. There is
    // no negotiated session state to hand across the `exec`, and fabricating a default one would
    // resume the successor claiming a protocol version and capability set the client never sent, so
    // such a session consumes the signal without upgrading: it keeps serving until the client
    // disconnects, after which the next launch picks up the new binary. Consuming rather than
    // leaving the signal unhandled is what keeps the fleet trigger from killing the server.
    //
    // `peer_info()` yields `Option<Arc<_>>`; deref-and-clone to the owned `InitializeRequestParams`
    // the `Upgrade`/handoff want.
    let peer_info = running.peer().peer_info().map(|p| (*p).clone());
    if let Some(mut sigusr1) = sigusr1 {
        // `sigusr1` is armed only when `install_path` is set, so `zip` drops nothing else.
        let upgrade = install_path.zip(peer_info).map(|(install_path, peer_info)| {
            let negotiated_protocol_version = peer_info.protocol_version.as_str().to_string();
            let handoff_dir = config
                .database
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(std::env::temp_dir);
            Upgrade {
                gate: Arc::clone(&gate),
                inflight,
                install_path,
                handoff_dir,
                peer_info,
                negotiated_protocol_version,
            }
        });
        if upgrade.is_none() {
            eprintln!(
                "hot-upgrade: client connected without `initialize` (stateless lifecycle); no \
                 session state to hand off, so SIGUSR1 will be observed and ignored"
            );
        }
        tokio::spawn(async move {
            // On success `run()` never returns (it `exec`s or exits); on a drain-timeout abort it
            // returns and we wait for the next signal.
            while sigusr1.recv().await.is_some() {
                if let Some(upgrade) = &upgrade {
                    upgrade.run().await;
                }
            }
        });
    }

    running.waiting().await?;
    Ok(())
}

/// Sync is an active-repo capability: a dormant MCP server has neither a database nor an endpoint.
fn start_sync_host(config: Config) -> Option<rag_rat_core::sync_driver::ResidentSyncHost> {
    match rag_rat_core::sync_driver::ResidentSyncHost::start(config) {
        Ok(host) => host,
        Err(error) => {
            tracing::warn!(%error, "resident sync host did not start");
            None
        },
    }
}
