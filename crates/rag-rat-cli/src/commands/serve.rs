use rag_rat_base::config::Config;
use rag_rat_core::IndexDatabase;

use crate::cli::ServeArgs;

pub(crate) fn serve_http(config: Config, args: &ServeArgs) -> anyhow::Result<()> {
    if (!args.bind.is_loopback() || args.advertise_url.is_some())
        && (args.token_env.is_none() || args.allow_origin.is_empty())
    {
        anyhow::bail!(
            "non-loopback `rag-rat serve` requires --token-env and at least one --allow-origin"
        );
    }
    let token = match args.token_env.as_deref() {
        Some(name) => std::env::var(name)
            .map_err(|_| anyhow::anyhow!("--token-env variable `{name}` is missing"))?
            .trim()
            .to_string(),
        None => rag_rat_mcp::lens_server::ownership_token()?,
    };
    anyhow::ensure!(!token.is_empty(), "lens bearer token must not be empty");
    // Election BEFORE side effects: a second `rag-rat serve` on the same worktree must fail
    // fast without healing the index or spawning a watcher it never uses.
    let workspace_root = rag_rat_mcp::lens_server::workspace_root(&config);
    let election_lock = rag_rat_base::locks::FileLock::try_acquire(
        &rag_rat_base::locks::lens_server_lock_path_for(&config, &workspace_root),
    )?
    .ok_or_else(|| anyhow::anyhow!("a lens server already owns this worktree"))?;
    drop(IndexDatabase::open_config(&config)?);
    let _watcher = rag_rat_core::watch::Watcher::spawn(config.clone());
    let address = std::net::SocketAddr::new(args.bind, args.port);
    let allowed_origins = args.allow_origin.clone();
    let advertise_url = args.advertise_url.clone();
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async move {
        rag_rat_mcp::lens_server::serve_standalone(
            config,
            workspace_root,
            address,
            rag_rat_mcp::lens_server::StandaloneServeOptions {
                auth_token: token,
                allowed_origins,
                advertise_url,
            },
            election_lock,
            shutdown_signal(),
        )
        .await
    })
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}
