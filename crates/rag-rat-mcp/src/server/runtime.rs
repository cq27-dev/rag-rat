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
        let service = RagRatService::new(config, output_format).serve(stdio()).await?;
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
    let running = RagRatService::new_dormant(output_format).serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// Aborts a spawned task when dropped. Used to tear down the hook listener on normal EOF
/// shutdown so its socket + election lock release promptly (a hot-`exec` replaces the process
/// image instead, so the task vanishes and the successor re-elects).
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
    use tokio::signal::unix::{SignalKind, signal};

    use crate::upgrade::{self, GatedStdin, Upgrade, UpgradeGate};

    let gate = UpgradeGate::new();
    let service = RagRatService::new(config.clone(), output_format);
    let inflight = service.inflight();

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
    let install_path = upgrade::install_path();
    let _watcher =
        rag_rat_core::watch::Watcher::spawn_with_fleet(config.clone(), install_path.clone());

    // Grep-augment hook listener: one elected listener per worktree. Spawned after the `running`
    // match so it covers both cold start and hot-upgrade resume. On normal EOF shutdown the guard
    // aborts the task so the socket + election lock release promptly; on a hot-`exec` the process
    // image is replaced (task vanishes) and the new process re-elects.
    let _hook_listener = AbortOnDrop(crate::agent_hook::spawn_listener(config.clone()));

    // Arm the SIGUSR1 hot-upgrade handler only when an install target is configured.
    if let Some(install_path) = install_path {
        // rmcp 1.8 changed `peer_info()` from `Option<&_>` to `Option<Arc<_>>`; deref-and-clone to
        // the owned `InitializeRequestParams` the `Upgrade`/handoff want. `*p` derefs either form,
        // so this compiles on 1.7 and 1.8 alike.
        let peer_info = running.peer().peer_info().map(|p| (*p).clone()).unwrap_or_default();
        let negotiated_protocol_version = peer_info.protocol_version.as_str().to_string();
        let handoff_dir = config
            .database
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let upgrade = Upgrade {
            gate: Arc::clone(&gate),
            inflight,
            install_path,
            handoff_dir,
            peer_info,
            negotiated_protocol_version,
        };
        tokio::spawn(async move {
            let Ok(mut sigusr1) = signal(SignalKind::user_defined1()) else {
                eprintln!("hot-upgrade: could not install SIGUSR1 handler; disabled");
                return;
            };
            // On success `run()` never returns (it `exec`s or exits); on a drain-timeout abort it
            // returns and we wait for the next signal.
            while sigusr1.recv().await.is_some() {
                upgrade.run().await;
            }
        });
    }

    running.waiting().await?;
    Ok(())
}
