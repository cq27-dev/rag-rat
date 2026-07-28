//! The live oracle's maintenance-pass tail (#74 slice 2 / #534): the watcher-owned state that
//! drives [`rag_rat_oracle::live_oracle_pass`] — the resident LSP session (lazily spawned,
//! idle-shutdown) and the backlog of changed paths a prior pass's request budget didn't reach.
//!
//! Gating (all cheap, in order): `[oracle.live] enabled` (standalone — it does NOT imply or
//! require `[oracle] auto_run`), Rust among the checkout's indexed languages, and a non-empty
//! worklist (backlog ∪ this pass's changed `.rs` paths). A pass with no reliable changed set
//! (a heal/bootstrap leaves `clone_delta_hint = None`) contributes no paths: live's scope is
//! exactly "files the pass reindexed", and a whole-checkout sweep is the BATCH pass's job.
//!
//! Everything here is best-effort: a missing `rust-analyzer`, a failed spawn, a warming server,
//! or a dead server mid-pass never fails the maintenance pass — the worklist rides to the next
//! pass via the backlog, and an aborted session is dropped so a later pass respawns. Bounded
//! respawn backoff and idle scheduling independent of maintenance passes remain in #535.

use std::collections::{BTreeSet, HashSet};

use rag_rat_base::config::Config;
use rag_rat_base::language::Language;
use rag_rat_base::time::now_ms;
use rag_rat_oracle::LiveOracleSession;

use crate::index::IndexDatabase;

/// Resident live-oracle state for one watcher: the LSP session + the deferred-path backlog.
/// Owned by the pass-worker closure (it must outlive individual passes); the hook/CLI
/// `maintenance_pass*` entry points pass no state, so the live stage only ever runs from the
/// resident watcher — a one-shot CLI pass must not spawn a minutes-warming language server.
pub(crate) struct LiveOracleTail {
    session: Option<LiveOracleSession>,
    backlog: Vec<String>,
}

impl LiveOracleTail {
    pub(crate) fn new() -> Self {
        Self { session: None, backlog: Vec::new() }
    }

    /// One pass's live stage: the idle-shutdown sweep, then (when enabled and there is work)
    /// resolve the changed Rust files through the resident client. Never returns an error — a
    /// failure is logged and the work rides the next pass.
    pub(crate) fn on_pass(
        &mut self,
        db: &IndexDatabase,
        config: &Config,
        changed_paths: Option<&BTreeSet<String>>,
    ) {
        let live_cfg = &config.oracle.live;
        if !live_cfg.enabled {
            return;
        }
        let now = now_ms();
        // Idle-shutdown sweep: an idle server shouldn't hold rust-analyzer's resident memory.
        // Runs even on a workless (quiet) pass — that is exactly when idleness accrues.
        if self.session.as_ref().is_some_and(|session| {
            session.idle_for(now, live_cfg.idle_shutdown_secs.saturating_mul(1000))
        }) && let Some(session) = self.session.take()
        {
            tracing::debug!(target: "rag_rat_core::watch", "live oracle: idle shutdown");
            session.shutdown();
        }

        // The worklist: backlog first (older edits wait longest), then this pass's changed Rust
        // paths, deduped. `changed_paths` is `None` on a heal/bootstrap — no reliable superset,
        // so only the backlog rides.
        let worklist = assemble_worklist(std::mem::take(&mut self.backlog), changed_paths);
        if worklist.is_empty() {
            return;
        }
        // Rust must be an indexed language of this checkout (ra-lsp's language).
        if !config.targets.iter().any(|target| target.language == Language::Rust) {
            return;
        }

        // Spawn the session lazily on the first eligible pass. `None` (rust-analyzer absent /
        // unw spawnable) leaves the work in the backlog for a later pass — the same
        // degrade-quietly UX as a missing embedding model.
        if self.session.is_none() {
            self.session = LiveOracleSession::spawn(&config.root, now);
        }
        let Some(session) = &mut self.session else {
            self.backlog = worklist;
            return;
        };

        match db.run_live_oracle_pass(session, &worklist, live_cfg.max_requests_per_pass, now) {
            Ok(report) => {
                self.backlog = report.unfinished_paths.clone();
                // An aborted pass means the server died or wedged mid-resolution: drop the
                // session so the next pass respawns a clean one instead of reusing a broken
                // transport (the aborted files are already requeued in `unfinished_paths`).
                if report.status.starts_with("Aborted:")
                    && let Some(_aborted_session) = self.session.take()
                {
                    // Let the binding hard-kill on Drop; graceful shutdown would attempt another
                    // bounded request against the same wedged transport.
                    tracing::warn!(
                        target: "rag_rat_core::watch",
                        status = %report.status,
                        "live oracle: server aborted; session dropped, respawn on next pass"
                    );
                }
                if report.rows_written > 0 || !report.unfinished_paths.is_empty() {
                    tracing::info!(
                        target: "rag_rat_core::watch",
                        rows_written = report.rows_written,
                        upgraded = report.upgraded,
                        confirmed = report.confirmed,
                        contradicted = report.contradicted,
                        requests = report.requests_used,
                        deferred = report.unfinished_paths.len(),
                        refinements_invalidated = report.refinements_invalidated,
                        status = %report.status,
                        "live oracle pass"
                    );
                }
            },
            Err(err) => {
                // A DB-side failure (the only `Err`): drop the session so the next pass
                // respawns fresh, keep the whole worklist riding, and never fail the pass.
                if let Some(session) = self.session.take() {
                    session.shutdown();
                }
                self.backlog = worklist;
                tracing::warn!(
                    target: "rag_rat_core::watch",
                    error = %err,
                    "live oracle pass failed; worklist deferred to the next pass"
                );
            },
        }
    }
}

/// The pass's worklist: backlog paths first (oldest edits wait longest), then the changed
/// `.rs` paths (ra-lsp's language), deduped, order-preserving. `None` changed paths (a
/// heal/bootstrap pass) contributes nothing — only the backlog rides.
fn assemble_worklist(
    backlog: Vec<String>,
    changed_paths: Option<&BTreeSet<String>>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut worklist = Vec::new();
    for path in backlog {
        if seen.insert(path.clone()) {
            worklist.push(path);
        }
    }
    if let Some(paths) = changed_paths {
        for path in paths.iter().filter(|p| p.ends_with(".rs")) {
            if seen.insert(path.clone()) {
                worklist.push(path.clone());
            }
        }
    }
    worklist
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worklist_dedupes_backlog_against_changed_and_filters_non_rust() {
        let backlog = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let changed = BTreeSet::from([
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
            "Cargo.toml".to_string(),
        ]);
        let worklist = assemble_worklist(backlog, Some(&changed));
        // Backlog order first, then new changed paths; duplicates collapse; non-Rust dropped.
        assert_eq!(worklist, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    }

    #[test]
    fn worklist_without_changed_set_rides_backlog_only() {
        let backlog = vec!["src/a.rs".to_string()];
        // A heal/bootstrap pass (None) contributes no paths.
        assert_eq!(assemble_worklist(backlog, None), vec!["src/a.rs"]);
        assert!(assemble_worklist(Vec::new(), None).is_empty());
    }
}
