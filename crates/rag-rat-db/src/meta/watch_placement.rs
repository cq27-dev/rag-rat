//! The watcher's out-of-band watch-placement failure flush: persist the failure high-water mark
//! without creating, migrating, or blocking on the index.

use rag_rat_base::config::Config;

use super::{WATCH_PLACEMENT_FAILURES_META, bump_repo_meta_high_water};
use crate::storage::IndexConnection;

/// Whether an out-of-band placement-failure flush is done with its count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushOutcome {
    /// The count is now in the DB, or there is definitively nothing to persist into (no index file
    /// yet, repo not registered, schema not Compatible), so retrying is pointless.
    Settled,
    /// A TRANSIENT `SQLITE_BUSY` skip: the caller should retry (the event-loop drain re-attempts
    /// next tick).
    BusySkip,
}

/// Persist a watch-placement failure high-water mark for the watcher's out-of-band flush paths —
/// the non-blocking post-resync flush on the event loop and the shutdown flush (#658 review). It is
/// deliberately CHEAP, NON-BLOCKING, and SIDE-EFFECT-FREE:
/// - **No index creation.** Returns early (no write) when the DB file does not exist, and opens
///   NON-creating ([`IndexConnection::open_read_write_no_create_nowait`]) — a first-time-empty
///   checkout that never built an index must not gain a schemaless `.rag-rat/index.sqlite` here.
/// - **No blocking.** The no-wait open + `busy_timeout = 0` mean a concurrent writer (another repo
///   in a consolidated DB, a checkpoint) yields `SQLITE_BUSY`, treated as SKIP — the event loop
///   must never stall on classification/fleet triggers, and the count rides the next pass.
/// - **No on-open heals.** Unlike `open_config` (schema migration, graph-index / model-manifest /
///   generated-flags heals), so a degraded watcher exiting after a binary/schema-version change
///   can't spend an unbounded heal here.
///
/// Config-SCOPED via [`crate::schema::resolve_config_repo_id`] so it targets the SAME repo the
/// pass's persist did, even in a consolidated multi-repo DB (a bare sole-repo pick could hit a
/// sibling). Also skips when the repo isn't registered yet (nothing to surface into) or the schema
/// isn't Compatible (the next full pass migrates + persists). The caller owns the per-repo write
/// lock. A non-busy error propagates.
pub fn record_watch_placement_failures_scoped(
    config: &Config,
    failures: u64,
) -> anyhow::Result<FlushOutcome> {
    // A first-time-empty checkout has no index yet: skip WITHOUT opening, so the flush never
    // creates a schemaless DB file (which would break the friendly no-index read path). Settled — a
    // later pass registers the repo and persists once real content appears.
    if !config.database.try_exists().unwrap_or(false) {
        return Ok(FlushOutcome::Settled);
    }
    match write_watch_placement_high_water(config, failures) {
        Ok(()) => Ok(FlushOutcome::Settled),
        // No-wait open/write: a busy DB (concurrent writer / checkpoint) is a TRANSIENT skip — the
        // caller retries (the count rides the next tick, pass, sweep, or shutdown flush). A file
        // that vanished in the race between the check above and the open surfaces as a non-busy
        // open error, propagated for the caller to log (best-effort).
        Err(err) if crate::storage::is_busy(&err) => Ok(FlushOutcome::BusySkip),
        Err(err) => Err(err),
    }
}

fn write_watch_placement_high_water(config: &Config, failures: u64) -> anyhow::Result<()> {
    let storage = IndexConnection::open_read_write_no_create_nowait(&config.database)?;
    let conn = storage.connection();
    if crate::schema::status(conn)?.state != crate::schema::SchemaState::Compatible {
        return Ok(());
    }
    let Some(repo_id) = crate::schema::resolve_config_repo_id(
        conn,
        &config.root,
        config.repo_id_override.as_deref(),
    )?
    else {
        return Ok(());
    };
    bump_repo_meta_high_water(conn, &repo_id, WATCH_PLACEMENT_FAILURES_META, failures)?;
    Ok(())
}
