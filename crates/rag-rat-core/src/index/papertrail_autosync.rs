//! Cross-process papertrail auto-sync orchestration: one coalesced mirror flight per repository,
//! shared by every trigger source — the watcher's periodic deadline, git-hook maintenance, and
//! any number of concurrent processes. The flight lock + pending marker follow the maintenance
//! coalescing pattern (#267), with one addition: the marker carries the strongest coalesced
//! [`AutosyncRequest`] so a queued full walk is never weakened by a later incremental trigger.
//!
//! The flight NEVER holds the repository write lock: mirror runs wait on the network (page
//! fetches, rate-governor sleeps) for arbitrarily long, and every database commit inside them is
//! a short synchronous transaction serialized by SQLite itself. Ordinary index maintenance and
//! this flight therefore proceed independently.

use rag_rat_base::config::Config;
use rag_rat_base::locks::{self, FileLock};
use rag_rat_base::single_flight::{FlightOutcome, SingleFlight, Step};
use rag_rat_papertrail::{AutosyncRequest, PapertrailContext, PapertrailSyncReport};

use crate::index::IndexDatabase;

/// What one auto-sync trigger produced.
#[derive(Debug)]
pub enum AutosyncOutcome {
    /// The repo resolves no tracker bindings; there is nothing to sync.
    Disabled,
    /// The repo has never been indexed. Automatic sync serves already-indexed repos only — a
    /// deferred first-time-empty hook, or a shared database where this repo was only ever
    /// registered read-only, must not start mirroring. The request is queued in the pending
    /// marker so the first post-index trigger runs with it.
    NotIndexed,
    /// Another flight holds this repository's lock. The request was merged into the pending
    /// marker; the holder's follow-up pass (or the next periodic deadline) covers it.
    Coalesced,
    /// This process ran the flight, absorbing any coalesced follow-ups; the LAST run's report.
    Ran(Box<PapertrailSyncReport>),
}

/// Run — or coalesce into — the repository's single papertrail flight. Callers treat this as
/// best-effort: per-binding provider failures are persisted as binding health inside the run and
/// never surface here; only a process-level failure (the database cannot be opened, storage is
/// broken) returns `Err`, and it leaves the pending marker set so a later trigger retries.
pub fn run(config: &Config, request: AutosyncRequest) -> anyhow::Result<AutosyncOutcome> {
    // Resolve bindings from the config BEFORE opening the database: with no tracker bindings the
    // trigger is a no-op, and a per-commit git hook must not pay a database open to learn that.
    if PapertrailContext::resolve(config).trackers.is_empty() {
        return Ok(AutosyncOutcome::Disabled);
    }
    // The non-creating half of the indexed-only gate, BEFORE any open: opening a missing
    // database creates the (empty) file first and only then refuses on the missing schema, and
    // that artifact defeats every later `database.exists()` "build the index first" hint. A
    // repo with no store at all is trivially not indexed — but the accepted signal is queued
    // (the first post-index trigger absorbs it) so a first index pass racing this trigger can't
    // lose it.
    if !config.database.exists() {
        let lock_repo = locks::write_lock_repo_id(config);
        flight(config, &lock_repo).queue(request)?;
        return Ok(AutosyncOutcome::NotIndexed);
    }
    // Identity re-key retry: when a pass discovers its flight lock was keyed from a
    // since-upgraded identity, the stranded request (absorbed from the old-key marker) is carried
    // to fresh keys and re-run — an `Incremental` change signal is not re-derivable from persisted
    // cursor state, so it must not be dropped. Transitions are one-time per repo; the cap only
    // bounds pathological churn.
    let mut request = request;
    for _ in 0..3 {
        let lock_repo = locks::write_lock_repo_id(config);
        match flight(config, &lock_repo)
            .run(request, |queued| run_pass(config, &lock_repo, *queued))?
        {
            FlightOutcome::Coalesced => return Ok(AutosyncOutcome::Coalesced),
            FlightOutcome::Ran(Some(report)) => return Ok(AutosyncOutcome::Ran(report)),
            // Defensive: unreachable with a non-`None` initial (the first pass reports, defers,
            // re-keys, or errors before the drain can exit empty-handed).
            FlightOutcome::Ran(None) => return Ok(AutosyncOutcome::Coalesced),
            // `StopRequeue` (NotIndexed) left the signal in the marker for a future trigger.
            FlightOutcome::Stopped(None) => return Ok(AutosyncOutcome::NotIndexed),
            // `StopCarry` (Rekeyed) handed the stranded request (+ absorbed marker) back to re-key.
            FlightOutcome::Stopped(Some(stranded)) => request = stranded,
        }
    }
    // Pathological identity churn: preserve the signal where fresh-keyed triggers will find it,
    // and surface the condition to the caller's log.
    preserve_for_fresh_key(config, request);
    anyhow::bail!(
        "the repo identity kept changing across papertrail flight attempts; the request was \
         queued for the next trigger"
    )
}

/// This repo's papertrail single-flight coordinator, keyed to `lock_repo` (the currently-resolved
/// identity). Rebuilt per attempt: the identity can upgrade mid-flight (a shallow clone resolving
/// its portable id), which re-keys every flight path.
fn flight(config: &Config, lock_repo: &str) -> SingleFlight<AutosyncRequest> {
    SingleFlight::for_flight(locks::FlightKind::Papertrail, &config.database, lock_repo)
}

/// One scheduled flight pass under the held flight lock, mapped to a single-flight [`Step`]:
/// - the repo's identity key went stale since the lock was taken → `StopCarry` (a fresh-keyed
///   flight is now possible; re-key and retry rather than mirror twice over one cursor);
/// - the repo is not indexed yet → `StopRequeue` (automatic sync serves indexed repos only; leave
///   the accepted signal for the first post-index trigger — the deliberate marker-without-a-runner
///   exception);
/// - otherwise the scheduled pass runs → `Ran`.
fn run_pass(
    config: &Config,
    lock_repo: &str,
    request: AutosyncRequest,
) -> anyhow::Result<Step<Box<PapertrailSyncReport>>> {
    let db = IndexDatabase::open_config(config)?;
    // Identity re-key guard on EVERY pass at the latest pre-walk moment: the flight lock was keyed
    // from the identity resolved at entry, and both the wait for the slot and `open_config`'s own
    // git reads take real time. A fresh resolution here is exactly what a future trigger keys its
    // lock from, so a mismatch means a concurrent fresh-keyed flight is possible.
    if locks::write_lock_repo_id(config) != lock_repo {
        return Ok(Step::StopCarry);
    }
    // `open_config` registers read-only (it never creates an index), so on a shared database this
    // repo can be registered yet never indexed; the #427 "an index pass ran here" signal separates
    // the two.
    if !rag_rat_db::schema::is_root_already_indexed_conn(db.storage.connection(), config)? {
        return Ok(Step::StopRequeue);
    }
    Ok(Step::Ran(Box::new(db.papertrail_sync_scheduled(request)?)))
}

/// Best-effort: queue a request stranded by an identity re-key into the CURRENT identity's
/// pending marker, where fresh-keyed triggers absorb it. The marker sits until the next trigger
/// (the watcher deadline bounds the wait) — the same bounded staleness every error-path marker
/// accepts.
fn preserve_for_fresh_key(config: &Config, request: AutosyncRequest) {
    let lock_repo = locks::write_lock_repo_id(config);
    let _ = flight(config, &lock_repo).queue(request);
}

/// The explicit `papertrail sync` command: unconditional manual semantics (every binding
/// dispatched, reference discovery refreshed, `full` honored), SHARING the per-repo flight lock
/// with automatic sync — two mirror runs over one binding would interleave their cursor
/// load/save cycles and clobber each other's walk state. Unlike automatic triggers, a manual
/// invocation never degrades into a policy-gated follow-up: when an automatic flight is in the
/// air it WAITS for the lock (announcing the wait through `on_wait`, interruptible like any
/// foreground command) and then runs the full manual pass itself.
pub fn run_manual(
    config: &Config,
    full: bool,
    mut on_wait: impl FnMut(),
) -> anyhow::Result<PapertrailSyncReport> {
    // Refuse BEFORE any lock or open: opening a missing database creates the (empty) file
    // first and only then refuses on the missing schema, and that artifact defeats every later
    // `database.exists()` "build the index first" hint.
    anyhow::ensure!(
        config.database.exists(),
        "no index at this path yet; build one with `rag-rat index` or `rag-rat index --full`"
    );
    // The same identity re-key guard as `run_pass`, with manual semantics: the identity can
    // move between resolving the lock key and the post-open re-check (the wait for the flight
    // slot and `open_config`'s own git reads take real time, and the open can itself upgrade
    // the identity). An automatic runner steps aside; an explicit command instead RE-KEYS and
    // retries under the fresh identity so the user still gets their unconditional pass. A
    // transition is one-time per repo — the cap only bounds pathological churn.
    for _ in 0..3 {
        let lock_repo = locks::write_lock_repo_id(config);
        let sf = flight(config, &lock_repo);
        // No marker lock is held while blocking here (a contender holding the marker lock only
        // TRY-acquires the flight lock — see the single-flight lock ordering); the runner's exit
        // handoff releases the flight lock only with an empty marker, so this wait ends at a clean
        // boundary.
        let flight_lock = match FileLock::try_acquire(sf.flight_lock_path())? {
            Some(flight_lock) => flight_lock,
            None => {
                on_wait();
                FileLock::acquire_blocking(sf.flight_lock_path())?
            },
        };
        let db = IndexDatabase::open_config(config)?;
        if locks::write_lock_repo_id(config) != lock_repo {
            // Never mirror under a stale-keyed flight lock: a trigger resolving the upgraded
            // identity keys a DIFFERENT lock and could run concurrently over the same cursor.
            // A follow-up that coalesced into the OLD-key marker while this runner held the
            // stale lock is unreachable for fresh-keyed triggers — carry it forward before
            // releasing.
            drop(db);
            if let Some(stranded) = sf.take()? {
                preserve_for_fresh_key(config, stranded);
            }
            drop(flight_lock);
            continue;
        }
        let manual_report = db.papertrail_sync(full)?;
        drop(db);
        // Triggers that coalesced behind the manual pass still get their (scheduled) follow-up and
        // the exit handoff before the flight lock releases. A follow-up stranded by an identity
        // transition mid-drain is queued for fresh-keyed triggers instead of dropped.
        if let FlightOutcome::Stopped(Some(stranded)) =
            sf.drain(flight_lock, None, |queued| run_pass(config, &lock_repo, *queued))?
        {
            preserve_for_fresh_key(config, stranded);
        }
        return Ok(manual_report);
    }
    anyhow::bail!(
        "the repo identity kept changing while acquiring the papertrail flight lock; re-run \
         `rag-rat papertrail sync` once the checkout settles"
    )
}

#[cfg(test)]
#[path = "papertrail_autosync_tests.rs"]
mod tests;
