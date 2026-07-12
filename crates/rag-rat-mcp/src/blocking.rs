use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::ErrorData;
use rmcp::model::CallToolResult;
use tokio::sync::Semaphore;
use tokio::task::JoinError;

pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const DEFAULT_TOOL_WORKERS: usize = 2;
pub(crate) const TOOL_TIMEOUT_ENV: &str = "RAG_RAT_MCP_TOOL_TIMEOUT_SECS";
pub(crate) const TOOL_WORKERS_ENV: &str = "RAG_RAT_MCP_TOOL_WORKERS";

pub(crate) fn tool_timeout() -> Duration {
    std::env::var(TOOL_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| parse_tool_timeout(&raw))
        .unwrap_or(DEFAULT_TOOL_TIMEOUT)
}

pub(crate) fn parse_tool_timeout(raw: &str) -> Option<Duration> {
    let secs = raw.trim().parse::<u64>().ok()?;
    (secs > 0).then(|| Duration::from_secs(secs))
}

pub(crate) fn tool_workers() -> usize {
    std::env::var(TOOL_WORKERS_ENV)
        .ok()
        .and_then(|raw| parse_tool_workers(&raw))
        .unwrap_or(DEFAULT_TOOL_WORKERS)
}

pub(crate) fn parse_tool_workers(raw: &str) -> Option<usize> {
    let workers = raw.trim().parse::<usize>().ok()?;
    (workers > 0).then_some(workers)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolTimeoutPolicy {
    ReturnTimeout,
    WaitForCompletion,
}

impl ToolTimeoutPolicy {
    pub(crate) fn for_tool(name: &str) -> Self {
        if crate::tools::is_write_tool(name) {
            Self::WaitForCompletion
        } else {
            Self::ReturnTimeout
        }
    }
}

pub(crate) async fn run_blocking_tool(
    name: String,
    timeout: Duration,
    timeout_policy: ToolTimeoutPolicy,
    workers: Arc<Semaphore>,
    runner: impl FnOnce() -> Result<CallToolResult, ErrorData> + Send + 'static,
) -> Result<CallToolResult, ErrorData> {
    let started = Instant::now();
    tracing::debug!(
        target: "rag_rat_mcp::server",
        tool = %name,
        timeout_ms = timeout.as_millis(),
        "mcp tool call started"
    );

    let Some(wait_budget) = remaining_timeout(started, timeout) else {
        return tool_timeout_error(&name, started, timeout);
    };
    let permit = match tokio::time::timeout(wait_budget, workers.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) =>
            return Err(ErrorData::internal_error("tool worker limiter was closed", None)),
        Err(_) => return tool_timeout_error(&name, started, timeout),
    };

    let mut handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        runner()
    });
    let Some(run_budget) = remaining_timeout(started, timeout) else {
        return handle_elapsed_deadline(name, started, timeout, timeout_policy, &mut handle).await;
    };

    match tokio::time::timeout(run_budget, &mut handle).await {
        Ok(joined) => finish_blocking_tool(&name, started, joined),
        Err(_) =>
            handle_elapsed_deadline(name, started, timeout, timeout_policy, &mut handle).await,
    }
}

fn remaining_timeout(started: Instant, timeout: Duration) -> Option<Duration> {
    timeout.checked_sub(started.elapsed()).filter(|remaining| !remaining.is_zero())
}

async fn handle_elapsed_deadline(
    name: String,
    started: Instant,
    timeout: Duration,
    timeout_policy: ToolTimeoutPolicy,
    handle: &mut tokio::task::JoinHandle<Result<CallToolResult, ErrorData>>,
) -> Result<CallToolResult, ErrorData> {
    match timeout_policy {
        ToolTimeoutPolicy::ReturnTimeout => tool_timeout_error(&name, started, timeout),
        ToolTimeoutPolicy::WaitForCompletion => {
            tracing::warn!(
                target: "rag_rat_mcp::server",
                tool = %name,
                timeout_ms = timeout.as_millis(),
                duration_ms = started.elapsed().as_millis(),
                "mcp write tool exceeded timeout; waiting for blocking work to finish"
            );
            finish_blocking_tool(&name, started, handle.await)
        },
    }
}

fn finish_blocking_tool(
    name: &str,
    started: Instant,
    joined: Result<Result<CallToolResult, ErrorData>, JoinError>,
) -> Result<CallToolResult, ErrorData> {
    match joined {
        Ok(Ok(result)) => {
            tracing::debug!(
                target: "rag_rat_mcp::server",
                tool = %name,
                duration_ms = started.elapsed().as_millis(),
                "mcp tool call finished"
            );
            Ok(result)
        },
        Ok(Err(err)) => {
            tracing::warn!(
                target: "rag_rat_mcp::server",
                tool = %name,
                duration_ms = started.elapsed().as_millis(),
                error = ?err,
                "mcp tool call failed"
            );
            Err(err)
        },
        Err(err) => {
            tracing::error!(
                target: "rag_rat_mcp::server",
                tool = %name,
                duration_ms = started.elapsed().as_millis(),
                error = %err,
                "mcp tool call panicked"
            );
            Err(ErrorData::internal_error(format!("tool `{name}` panicked: {err}"), None))
        },
    }
}

fn tool_timeout_error(
    name: &str,
    started: Instant,
    timeout: Duration,
) -> Result<CallToolResult, ErrorData> {
    tracing::error!(
        target: "rag_rat_mcp::server",
        tool = %name,
        timeout_ms = timeout.as_millis(),
        duration_ms = started.elapsed().as_millis(),
        "mcp tool call timed out"
    );
    Err(ErrorData::internal_error(
        format!(
            "tool `{name}` timed out after {}s; see the rag-rat MCP log for details",
            timeout.as_secs()
        ),
        None,
    ))
}

#[cfg(test)]
mod tests;
