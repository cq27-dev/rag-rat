use super::run::with_oracle_write_lock;
use crate::open_index;

/// Background thread that keeps SCIP-grade ranking fresh for the long-lived MCP server without a
/// manual `oracle run`: poll on a sub-quiet-period cadence and, when the active checkout's index is
/// stale AND has been quiet long enough AND the min-interval floor has elapsed, run the oracle for
/// each known tool. Opt-in (`[oracle] auto_run`, default OFF) — returns immediately when disabled,
/// so short-lived CLI/hook commands (which never reach `Cmd::Mcp`) never spawn it.
///
/// Mirrors [`super::super::config_info::spawn_detached_version_refresh`]: detached, best-effort,
/// fail-open (`let _ = …`), dies with the process (a hot-upgrade re-exec re-spawns it). Each run
/// uses the SAME lock-free production path as `oracle run` — `produce_scip_with_tool` OUTSIDE the
/// write lock, only the pre-spawn snapshot + the join briefly serialized (#82/#83) — so the watcher
/// is never starved through the minutes-long subprocess, and the #82/#83 TOCTOU gates stay armed. A
/// `Blocked` outcome (tool not installed) is a no-op: the loop simply sleeps to the next tick
/// rather than spinning on it.
pub(crate) fn spawn_detached_oracle_auto_run(config: &rag_rat_base::config::Config) {
    use rag_rat_oracle::{AutoRunDecision, AutoRunInputs, OracleTool};
    if !config.oracle.auto_run {
        return;
    }
    // Poll well under the quiet period so a checkout that goes quiet is picked up within roughly
    // one quiet window, while the between-tick cost (a cheap meta + `oracle_runs` read, gated
    // by the pure decision before any subprocess) stays negligible. Floor the cadence so a tiny
    // configured quiet period can't busy-loop.
    let quiet_secs = config.oracle.auto_run_quiet_period_secs;
    let poll = std::time::Duration::from_secs((quiet_secs / 4).max(60));
    let quiet_period_ms = saturating_secs_to_ms(quiet_secs);
    let min_interval_ms = saturating_secs_to_ms(config.oracle.auto_run_min_interval_secs);
    let config = config.clone();
    std::thread::spawn(move || {
        loop {
            // Sleep BEFORE the first decision. This thread is spawned just before `run_stdio`
            // starts the file watcher, so an immediate first tick could run the oracle against the
            // pre-watcher index — missing any unindexed working-tree changes, recording them as
            // skipped/drifted documents, and then letting the min-interval gate block a corrected
            // run for hours. One poll interval lets the watcher's initial maintenance pass index
            // those changes first. (#142 review)
            std::thread::sleep(poll);
            // Each tick re-opens the index so a fresh `(commit_sha, worktree_id)` checkout (the
            // server outlives branch switches) and the latest `indexed_at_ms` are read anew. All
            // fail-open: any error just waits for the next tick.
            let _ = maybe_run_oracle_once(&config, quiet_period_ms, min_interval_ms);
        }
    });

    /// One throttled pass: read the staleness inputs for each tool, ask the pure gate, and on `Run`
    /// take the lock-free production path. Returns `Ok(())` even when nothing ran — the caller only
    /// uses it to swallow errors fail-open.
    fn maybe_run_oracle_once(
        config: &rag_rat_base::config::Config,
        quiet_period_ms: i64,
        min_interval_ms: i64,
    ) -> anyhow::Result<()> {
        let now_ms = rag_rat_base::time::now_ms();
        // `indexed_at_ms` is the active checkout's last index-change clock; without it we can't
        // judge staleness, so skip this tick.
        let last_index_change_ms = {
            let db = open_index(config)?;
            match db.status(&config.database)?.indexed_at_ms {
                Some(ms) => ms,
                None => return Ok(()),
            }
        };
        // The languages this checkout actually indexes. Gating background runs to these (#176)
        // stops the auto-run loop from invoking a backend whose language isn't present —
        // e.g. scip-python installed but no Python target: it would index nothing, fail,
        // the error would be swallowed with no `oracle_runs` row recorded, and the loop
        // would retry the doomed run every poll.
        let configured_languages: std::collections::HashSet<_> =
            config.targets.iter().map(|target| target.language).collect();
        for &tool in OracleTool::ALL {
            // Live-only backends (`ra-lsp`) are driven by the watcher, never by the batch
            // auto-run loop (#534).
            if !tool.batch_capable() {
                continue;
            }
            // Skip a backend whose language this checkout doesn't index — never auto-run it here
            // (the status registry stays broad; only background runs are gated).
            let manifest = rag_rat_oracle::ToolManifest::for_tool(tool);
            if !manifest.languages.iter().any(|lang| configured_languages.contains(lang)) {
                continue;
            }
            // Cheap probe before any decision: an uninstalled tool can never run, so don't even
            // read its run history.
            if matches!(
                rag_rat_oracle::probe_oracle_tool(tool),
                rag_rat_oracle::ToolAvailability::Blocked { .. }
            ) {
                continue;
            }
            let last_run_ms = {
                let db = open_index(config)?;
                db.latest_oracle_run_started_at(tool)?
            };
            let decision = rag_rat_oracle::auto_run_decision(AutoRunInputs {
                enabled: true,
                now_ms,
                last_index_change_ms,
                last_run_ms,
                quiet_period_ms,
                min_interval_ms,
            });
            if decision == AutoRunDecision::Run {
                let _ = run_oracle_tool_background(config, tool);
            }
        }
        Ok(())
    }

    /// The `oracle run` body for one tool, sans CLI output. The shared handoff
    /// snapshots the pre-spawn shas under the write lock, produces the
    /// `.scip` OUTSIDE the lock, then run only the join/write under the lock. A `Blocked`
    /// production is a no-op (returns `Ok`).
    fn run_oracle_tool_background(
        config: &rag_rat_base::config::Config,
        tool: OracleTool,
    ) -> anyhow::Result<()> {
        match super::run::produce_scip_outside_lock(config, tool, "rag-rat-oracle-auto")? {
            super::run::ScipHandoff::Blocked { .. } => Ok(()),
            super::run::ScipHandoff::Produced {
                started_at_ms,
                pre_spawn_sha,
                version,
                bytes,
                production_sha,
            } => {
                with_oracle_write_lock(config, |db| {
                    db.run_oracle_at(
                        tool,
                        &version,
                        &bytes,
                        rag_rat_oracle::ShaSnapshots {
                            production: Some(&production_sha),
                            pre_spawn: Some(&pre_spawn_sha),
                        },
                        started_at_ms,
                    )
                })?;
                Ok(())
            },
        }
    }

    /// Saturating seconds → ms for the throttle inputs (a wild config value can't overflow `i64`).
    fn saturating_secs_to_ms(secs: u64) -> i64 {
        i64::try_from(secs).unwrap_or(i64::MAX).saturating_mul(1000)
    }
}
