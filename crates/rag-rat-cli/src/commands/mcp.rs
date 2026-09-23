use super::config_info::spawn_detached_version_refresh;
use super::runtime_env::apply_embedding_runtime_env;
use super::spawn_detached_oracle_auto_run;
use crate::discover_config_optional;

/// Serve the stdio MCP server, tolerating the ABSENCE of a config. A globally-registered
/// `rag-rat mcp` is spawned in EVERY project, so outside a rag-rat repo it must serve a DORMANT
/// server (stays alive; every tool call returns a "no index here" notice) instead of exiting and
/// taking repo intelligence down for the whole session (#603). A config that is PRESENT but invalid
/// — or an explicit `--config` path that is missing — is still a loud error.
pub(crate) fn run_mcp(explicit: Option<&str>, json: bool) -> anyhow::Result<()> {
    // FIRST, before any other startup work: this process's environ already advertises it as a
    // hot-upgrade target, and the fleet trigger reads environs. Everything below — config
    // discovery, logging, the Tokio runtime the real handler needs — is time in which an
    // unhandled SIGUSR1 would kill the server outright instead of upgrading it.
    #[cfg(unix)]
    rag_rat_mcp::upgrade::suppress_sigusr1_until_armed();
    let output_format = super::format::output_format_from_json_flag(json);
    let config = discover_config_optional(explicit)?;
    // Repo-specific setup only when a config actually resolved. `_log` holds the tracing guard for
    // the server's lifetime; a dormant server writes no log (there is no repo to anchor it to).
    let _log = config.as_ref().map(|config| {
        apply_embedding_runtime_env(&config.llm.embedding.runtime);
        rag_rat_base::logging::init_logging(config, rag_rat_base::logging::Role::Mcp)
    });
    if let Some(config) = &config {
        // Detached, fail-open, dies with the process — no-ops unless opted in. Never spawned for a
        // dormant server (no repo to refresh or rank).
        spawn_detached_version_refresh(config);
        // A plugin update moves this server to a new release; carry the hooks along with it.
        // Fail-open: hooks are a freshness aid, never a reason not to serve.
        match crate::git_paths(&config.root)
            .and_then(|git| crate::hooks_support::refresh_managed_hooks(git.hooks_dir()))
        {
            Ok(refreshed) if !refreshed.is_empty() =>
                tracing::info!(?refreshed, "refreshed git hooks to this rag-rat version"),
            Ok(_) => {},
            Err(error) => tracing::warn!(%error, "could not refresh git hooks"),
        }
        spawn_detached_oracle_auto_run(config);
    }
    // Expose the plugin-cached binary on PATH (#1427). Runs for a dormant server too: that is the
    // first launch in a repo not set up yet, which is exactly when the user needs the CLI.
    match crate::path_shim::refresh_path_shim() {
        Ok(Some(shim)) => tracing::info!(shim = %shim.display(), "exposed rag-rat on PATH"),
        Ok(None) => {},
        Err(error) => tracing::warn!(%error, "could not update the rag-rat PATH shim"),
    }
    // Small worker pool: the stdio JSON-RPC loop is mostly serial and CPU-heavy indexing is rayon,
    // not tokio; stay multi_thread so a blocking tool handler can't stall the serve/upgrade tasks
    // (issue #63, facet 3).
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    // A resolved config → the active server; none → the dormant one. Kept as two calls so the
    // published `run_stdio(Config, …)` entry point stays source-compatible (#603).
    match config {
        Some(config) => runtime.block_on(rag_rat_mcp::server::run_stdio(config, output_format))?,
        None => runtime.block_on(rag_rat_mcp::server::run_stdio_dormant(output_format))?,
    }
    Ok(())
}
