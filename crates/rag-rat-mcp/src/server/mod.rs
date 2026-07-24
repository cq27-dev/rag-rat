//! MCP service dispatch and stdio runtime.

use std::sync::Arc;

use rag_rat_base::config::Config;
use tokio::sync::Semaphore;

use crate::blocking;

mod dispatch;
mod runtime;
mod service;

/// How often the stale-anchor rebind nudge may ride a tool result across the fleet (#752): once per
/// 30 minutes, unless a `memory_create`/`memory_update` forces it. Keeps the nudge off the vast
/// majority of tool calls so it stops inflating per-call tokens.
const NUDGE_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Clone)]
pub struct RagRatService {
    /// `None` ⇒ DORMANT: the server was launched outside any rag-rat repo (no `rag-rat.toml` at or
    /// above cwd). It still speaks MCP so a globally-registered server stays alive, but every tool
    /// call returns the dormant notice ([`dormant_tool_result`]) instead of touching an index.
    /// `Some` ⇒ the active server bound to a resolved repo config.
    config: Option<Config>,
    /// Output format for tool results. Defaults to TOON (denser for the LLM that reads them); set
    /// to JSON when the server was launched as `rag-rat mcp --json` — MCP has no per-call flag, so
    /// the choice is made once at launch (the CLI `--json` flag flows in via `run_stdio`).
    output_format: rag_rat_core::OutputFormat,
    /// In-flight tool-call counter, observed by the hot-upgrade teardown so it drains at a request
    /// boundary before `exec`. Present only on Unix, where hot-upgrade is supported.
    #[cfg(unix)]
    inflight: std::sync::Arc<crate::upgrade::Inflight>,
    tool_workers: Arc<Semaphore>,
    /// Per-agent (= per process) record of meta this session already saw — drive-by memories and
    /// static caveats — so re-surfacing them can be trimmed (#752). An `Arc` so every `Clone` of
    /// the service (each tool call clones it) shares one seen-set for the whole session.
    agent_seen: Arc<crate::output_trim::AgentSeen>,
}

impl RagRatService {
    /// The ACTIVE server bound to a resolved repo config. Published constructor — its
    /// `new(Config, …)` signature is preserved for source compatibility; the config-less DORMANT
    /// server is built via [`RagRatService::new_dormant`] instead of widening this to `Option`.
    pub fn new(config: Config, output_format: rag_rat_core::OutputFormat) -> Self {
        Self::with_optional_config(Some(config), output_format)
    }

    /// The DORMANT server (launched outside any rag-rat repo): no config, so every tool call
    /// returns [`RagRatService::dormant_tool_result`]. Separate from [`RagRatService::new`] so
    /// that constructor's published signature stays source-compatible (#603).
    pub(crate) fn new_dormant(output_format: rag_rat_core::OutputFormat) -> Self {
        Self::with_optional_config(None, output_format)
    }

    fn with_optional_config(
        config: Option<Config>,
        output_format: rag_rat_core::OutputFormat,
    ) -> Self {
        Self {
            config,
            output_format,
            #[cfg(unix)]
            inflight: crate::upgrade::Inflight::new(),
            tool_workers: Arc::new(Semaphore::new(blocking::tool_workers())),
            agent_seen: Arc::new(crate::output_trim::AgentSeen::default()),
        }
    }

    /// Shared in-flight counter, so the hot-upgrade signal task can wait for tool calls to drain.
    #[cfg(unix)]
    pub fn inflight(&self) -> std::sync::Arc<crate::upgrade::Inflight> {
        std::sync::Arc::clone(&self.inflight)
    }
}

pub use runtime::{run_stdio, run_stdio_dormant};

#[cfg(test)]
mod tests;
