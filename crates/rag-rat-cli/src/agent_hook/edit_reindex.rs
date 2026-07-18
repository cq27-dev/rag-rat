//! The detached edit-driven reindex runner (#661): the second half of the PostToolUse edit trigger.
//!
//! `posttooluse` (in the parent module) spawns `rag-rat edit-reindex --paths <edited files>` as a
//! DETACHED child and returns immediately, so the agent's tool call is never blocked. This module
//! is that child. It:
//!
//! - COALESCES a burst of concurrent edit hooks via the #660 single-flight — one runner holds the
//!   flight lock and reindexes; the rest merge their edited path(s) into the marker and exit, and
//!   the runner drains the union. A refactor firing many PostToolUse events runs ~one scoped pass,
//!   not N serialized multi-second processes.
//! - runs a STRUCTURAL-ONLY scoped pass ([`rag_rat_core::watch::reindex_paths`], the #659
//!   substrate): discover / parse / graph / FTS for exactly the edited paths (a linked-worktree
//!   edit routes through that checkout's overlay). It does NOT generate embeddings — the structural
//!   pass only seeds the embedding-model row; embedding generation is a separate step (`rag-rat
//!   reconcile` / the watcher / periodic), so there is no per-edit ONNX load or remote call.
//! - takes the write lock with a poll-retry TIMEOUT (never blocking): a contending index /
//!   maintenance pass is finite and may have ALREADY scanned the edited file, so the runner waits
//!   it out and runs its OWN scoped pass rather than assume the holder covers the edit; only a
//!   pathological over-budget hold defers the paths to the next trigger.
//!
//! Best-effort throughout: every path returns `Ok(())` so the (already-detached) process exits 0.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rag_rat_base::config::Config;
use rag_rat_base::locks::{self, WriteLock};
use rag_rat_base::single_flight::{FlightPayload, SingleFlight, Step};

/// How long the runner poll-retries the per-repo write lock before it defers. Generous on purpose:
/// the contender is a FINITE index/maintenance pass (git-hook maintenance is itself capped at
/// ~30s), not a network wait, and it may have ALREADY scanned the just-edited file before the edit
/// landed — so the runner must wait out the (finite) hold and run its OWN scoped pass rather than
/// assume the holder covers the edit. `acquire_timeout` polls (never blocks) for the whole budget;
/// the runner is detached, so this wait never touches the agent. Only a pathological longer hold
/// falls through to the leave-for-the-next-trigger deferral below.
const WRITE_LOCK_RETRY_BUDGET: Duration = Duration::from_secs(60);

/// The coalesced set of edited paths carried through the #660 single-flight marker.
///
/// `merge` is a set UNION so a runner draining the marker covers every path queued while it was
/// mid-pass (never a lost edit). On the wire the paths are newline-joined: a hook-supplied
/// `file_path` never contains a newline, and a pathological one merely splits into separate entries
/// that [`rag_rat_core::watch::reindex_paths`] no-ops on (an unmatched path is a no-op).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PathSet(BTreeSet<PathBuf>);

impl FlightPayload for PathSet {
    fn merge(mut self, other: Self) -> Self {
        self.0.extend(other.0);
        self
    }

    fn encode(&self) -> Vec<u8> {
        self.0
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes()
    }

    fn decode(encoded: &[u8]) -> Self {
        Self(
            String::from_utf8_lossy(encoded)
                .split('\n')
                .filter(|token| !token.is_empty())
                .map(PathBuf::from)
                .collect(),
        )
    }
}

/// `rag-rat edit-reindex` entry: discover the repo config from the (inherited) session cwd and run
/// the coalesced scoped reindex. Uses GOVERNING discovery (the same as the hook that spawned this),
/// so a linked-worktree edit with no branch-local config resolves the main config and routes
/// through the overlay. `None` config ⇒ not a rag-rat repo ⇒ silent no-op.
pub fn run(cwd: &Path, paths: &[PathBuf]) -> anyhow::Result<()> {
    let Some(config) = super::find_governing_config(cwd) else { return Ok(()) };
    run_with_config(&config, paths)
}

/// The coalescing runner, split out so tests drive it with a `Config` directly. Coalesced, ran, and
/// requeued-on-contention are all success from the trigger's view — the point is never to block or
/// error the agent.
fn run_with_config(config: &Config, paths: &[PathBuf]) -> anyhow::Result<()> {
    let initial: BTreeSet<PathBuf> = paths.iter().cloned().collect();
    if initial.is_empty() {
        return Ok(());
    }
    let lock_repo = locks::write_lock_repo_id(config);
    let flight = SingleFlight::<PathSet>::new(
        locks::edit_reindex_lock_path(&config.database, &lock_repo),
        locks::edit_reindex_pending_path(&config.database, &lock_repo),
        locks::edit_reindex_marker_lock_path(&config.database, &lock_repo),
    );
    flight.run(PathSet(initial), |paths| scoped_reindex(config, paths))?;
    Ok(())
}

/// One scoped structural pass under the held flight lock, mapped to a single-flight [`Step`]:
/// - the base index is not built yet (`EmptyIndexRefused`) → `Ran`: nothing to reconcile until the
///   first `rag-rat index`, so drop it (a first index pass covers these files);
/// - the write lock stays held past the whole retry budget → `StopRequeue`: leave the path union in
///   the marker for the next trigger to fold in and drain (an active session's subsequent edits do
///   this — the marker is never lost, only deferred);
/// - otherwise run the structural-only scoped reindex over the union → `Ran`.
fn scoped_reindex(config: &Config, paths: &PathSet) -> anyhow::Result<Step<()>> {
    let lock_repo = locks::write_lock_repo_id(config);
    // Poll-retry for the write lock across the whole budget (acquire_timeout never blocks). We do
    // NOT assume a concurrent index/maintenance pass covers the edit — it may have scanned the file
    // before the edit landed — so we wait out its finite hold and run our own scoped pass.
    let Some(_lock) =
        WriteLock::acquire_timeout(&config.database, &lock_repo, WRITE_LOCK_RETRY_BUDGET)?
    else {
        return Ok(Step::StopRequeue);
    };
    // Reentrant: `reindex_paths` re-acquires this same write lock (blocking) on this thread, which
    // the registry resolves to an instant depth-increment — the timeout above is the only wait.
    let paths: Vec<PathBuf> = paths.0.iter().cloned().collect();
    match rag_rat_core::watch::reindex_paths(config, &paths, |_| {}) {
        Ok(_) => Ok(Step::Ran(())),
        Err(error) if error.downcast_ref::<rag_rat_core::index::EmptyIndexRefused>().is_some() =>
            Ok(Step::Ran(())),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
