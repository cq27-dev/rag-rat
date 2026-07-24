use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use super::{NUDGE_TTL_MS, RagRatService};
use crate::blocking::{self, ToolTimeoutPolicy};

impl RagRatService {
    pub(super) fn call(&self, name: &str, value: Value) -> Result<CallToolResult, ErrorData> {
        // Dormant (launched outside a repo): return the notice. We deliberately do NOT re-discover
        // and serve a config that appears mid-session — a server without the active lifecycle
        // (watcher, git-hook freshness) could return results NOT validated against current source,
        // breaking rag-rat's core guarantee. Dormancy is binary: dormant, or fully active from
        // launch. The notice tells the user to restart the MCP server after `init` + `index`
        // (#603).
        let Some(config) = &self.config else {
            // A dormant server still rejects an UNKNOWN tool name exactly like an active one, so a
            // typo or stale tool surfaces as an error instead of being masked as `no_index`. Only a
            // KNOWN (advertised) tool earns the dormant notice.
            if !crate::tools::is_known_tool(name) {
                return Err(ErrorData::internal_error(format!("unknown tool `{name}`"), None));
            }
            return Ok(self.dormant_tool_result());
        };
        let mut value = crate::tools::call_tool_for_config(config, name, value)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        // Trim repeated meta to cut per-call tokens (#752): dedup drive-by memories this agent
        // already saw (to a tiny stub), throttle the static caveats, drop redundant per-edge flags.
        // TOON-only — it is a lossy transform (stubs a memory, drops default-valued fields), so it
        // stays off `--json` mode, whose whole purpose is stable, complete shapes for programmatic
        // clients (same reason the prose nudge is JSON-suppressed). And NEVER on the explicit
        // `memory_*` tools — there the agent asked for the memory, so it gets it in full.
        if self.output_format != rag_rat_core::OutputFormat::Json && !name.starts_with("memory_") {
            crate::output_trim::trim_result(&mut value, &self.agent_seen);
        }
        // MCP tool results are text content read by an LLM, so render TOON by default — it is
        // materially denser than JSON on the uniform-row payloads that dominate these tools, and
        // ties JSON on nested ones. `render` falls back to compact JSON on a TOON encode error, so
        // a tool result is never lost. JSON is reachable by launching `rag-rat mcp --json`
        // (the format is chosen once at launch; MCP has no per-call flag).
        let text = rag_rat_core::render(&value, self.output_format);
        let mut content = vec![ContentBlock::text(text)];
        if let Some(nudge) = self.stale_memory_nudge(name) {
            content.push(ContentBlock::text(nudge));
        }
        Ok(CallToolResult::success(content))
    }

    pub(super) async fn call_async(
        &self,
        name: String,
        value: Value,
    ) -> Result<CallToolResult, ErrorData> {
        let service = self.clone();
        let timeout = blocking::tool_timeout();
        let worker_name = name.clone();
        let timeout_policy = ToolTimeoutPolicy::for_tool(&name);
        let workers = Arc::clone(&self.tool_workers);
        // All tools funnel through this async chokepoint. Acquire the hot-upgrade in-flight guard
        // before queuing blocking work, then move it into the worker so a timed-out/detached read
        // still keeps the process from hot-execing until the blocking closure actually exits.
        #[cfg(unix)]
        let inflight = self.inflight.guard();
        blocking::run_blocking_tool(name, timeout, timeout_policy, workers, move || {
            #[cfg(unix)]
            let _inflight = inflight;
            service.call(&worker_name, value)
        })
        .await
    }

    /// Surface drifted repo-memory anchors to the AGENT as a second tool-result content block.
    /// Claude Code's agent is pull-based — MCP server notifications (`notifications/message`) reach
    /// the UI/logs but NOT the model's context (anthropics/claude-code#3174) — so a tool result is
    /// the one MCP-native channel that puts an actionable signal in front of the model. The nudge
    /// self-limits: once the agent runs `memory_rebind`, the count drops to 0 and it stops showing.
    ///
    /// THROTTLED (#752) to cut per-call tokens: it rides at most once per [`NUDGE_TTL_MS`] across
    /// the fleet (a shared last-shown timestamp in the sidecar store, claimed atomically so
    /// concurrent sessions don't both show), EXCEPT right after a `memory_create` /
    /// `memory_update` — the agent just touched memory, so a freshly-needed rebind is worth
    /// surfacing on that call regardless.
    ///
    /// Suppressed in `--json` mode: that mode exists for clients that parse the tool text AS JSON
    /// (or concatenate all text blocks), and a prose block would break them. The nudge is
    /// agent-directed prose, meaningful only in the default TOON (LLM-facing) mode.
    pub(super) fn stale_memory_nudge(&self, tool: &str) -> Option<String> {
        // No nudge in dormant mode (no index to read) or in `--json` mode (prose breaks JSON
        // clients).
        let config = self.config.as_ref()?;
        if self.output_format == rag_rat_core::OutputFormat::Json {
            return None;
        }
        let n = rag_rat_core::memory_attention_count(&config.database);
        if n == 0 {
            return None;
        }
        // A memory create/update forces the nudge (and resets the window); everything else is gated
        // on the throttle. The slot is claimed atomically — a `false` return means throttled or
        // another session just claimed it, so this call stays silent. Keyed by `repo_id` (the same
        // identity the per-repo write lock uses) so repos sharing a global DB don't mute each other
        // (#753 review); resolving it here, gated behind `n > 0`, keeps it off the common path.
        let force = matches!(tool, "memory_create" | "memory_update");
        let repo_id = rag_rat_base::locks::write_lock_repo_id(config);
        if !rag_rat_core::sidecar_state::take_memory_nudge_slot(
            &config.database,
            &repo_id,
            rag_rat_base::time::now_ms(),
            NUDGE_TTL_MS,
            force,
        ) {
            return None;
        }
        let noun = if n == 1 { "memory" } else { "memories" };
        Some(format!(
            "rag-rat: {n} active repo {noun} have stale/gone anchors. Call `memory_doctor` to \
             list them with suggested re-anchor targets, then `memory_rebind` to fix — so \
             source-anchored memory stays trustworthy for the next agent."
        ))
    }

    /// The result every tool call returns while the server is DORMANT (launched outside any rag-rat
    /// repo, and cwd STILL has no config). Rendered through the SAME output-format path as every
    /// tool result, so `--json` mode yields a directly-parseable JSON block (the JSON contract
    /// holds in dormant mode too) while the default TOON mode stays LLM-friendly. NON-error:
    /// the agent reads it as a normal response explaining how to enable an index.
    pub(super) fn dormant_tool_result(&self) -> CallToolResult {
        let payload = serde_json::json!({
            "status": "no_index",
            "message": "This rag-rat MCP server was started outside an indexed rag-rat repository, \
                        so it has no index to serve here.",
            "remedy": "Run the `init-rag-rat` skill to set this repo up conversationally, or run \
                       `rag-rat init` then `rag-rat index` in the repository root yourself. Either \
                       way, restart (reconnect) the rag-rat MCP server afterward so it activates \
                       against the new index.",
        });
        let text = rag_rat_core::render(&payload, self.output_format);
        CallToolResult::success(vec![ContentBlock::text(text)])
    }
}
