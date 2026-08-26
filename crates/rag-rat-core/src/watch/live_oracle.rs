//! The live oracle's maintenance-pass tail (#74 slice 2 / #534): the watcher-owned state that
//! drives [`rag_rat_oracle::live_oracle_pass`] — a resident LSP session per live backend (lazily
//! spawned, idle-shutdown) and the backlog of changed paths a prior pass's request budget didn't
//! reach.
//!
//! Gating (all cheap, in order): `[oracle.live] enabled` (standalone — it does NOT imply or
//! require `[oracle] auto_run`), the backend's language among the checkout's indexed languages,
//! and a non-empty worklist (backlog ∪ this pass's changed paths in that language). A pass with
//! no reliable changed set (a heal/bootstrap leaves `clone_delta_hint = None`) contributes no
//! paths: live's scope is exactly "files the pass reindexed", and a whole-checkout sweep is the
//! BATCH pass's job.
//!
//! Backends are INDEPENDENT: each keeps its own session, backlog, and respawn backoff, so a
//! wedged `rust-analyzer` never stalls TypeScript resolution (and vice versa), and each spends
//! its own request budget. A mixed-language repo therefore runs several resident servers when it
//! has several live-capable languages indexed.
//!
//! Everything here is best-effort: a missing language server, a failed spawn, a warming server,
//! or a dead server mid-pass never fails the maintenance pass — the worklist rides to the next
//! pass via the backlog, and an aborted session is dropped so a later pass respawns with bounded
//! backoff. The tail reports its next backlog-retry or idle-shutdown deadline to the event loop.

use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use rag_rat_base::config::Config;
use rag_rat_base::time::now_ms;
use rag_rat_oracle::{LiveBackend, LiveOracleSession, LivePassReport};

use crate::index::IndexDatabase;

const LIVE_ORACLE_RETRY_INTERVAL: Duration = Duration::from_secs(30);

// Test-only: force every spawn to behave as "no language server installed".
//
// The worktree regressions assert on what the stage DECIDED — which checkout it acted for, with
// which bindings, what it retained — not on what a server answered. Without this they silently
// depend on the developer's toolchain: with `rust-analyzer` on `PATH` a real session is launched
// and may drain the very backlog the assertions read, so the same test passes or fails according
// to what happens to be installed and how fast it warms.
//
// Thread-local because a test owns its thread under both runners this repo uses (nextest gives
// each test a process; the coverage job's `cargo test` gives each a thread).
#[cfg(test)]
thread_local! {
    static SUPPRESS_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn spawn_suppressed() -> bool {
    SUPPRESS_SPAWN.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn spawn_suppressed() -> bool {
    false
}

/// Suppress live-oracle spawns for as long as the returned guard is held (test-only).
#[cfg(test)]
pub(crate) fn suppress_live_spawn() -> SuppressSpawnGuard {
    SUPPRESS_SPAWN.with(|flag| flag.set(true));
    SuppressSpawnGuard
}

#[cfg(test)]
pub(crate) struct SuppressSpawnGuard;

#[cfg(test)]
impl Drop for SuppressSpawnGuard {
    fn drop(&mut self) {
        SUPPRESS_SPAWN.with(|flag| flag.set(false));
    }
}
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
struct RespawnBackoff {
    failures: u32,
    retry_at: Option<Instant>,
}

impl RespawnBackoff {
    fn ready(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|retry_at| now >= retry_at)
    }

    fn record_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let exponent = self.failures.saturating_sub(1).min(4);
        let delay =
            LIVE_ORACLE_RETRY_INTERVAL.saturating_mul(1 << exponent).min(RESPAWN_BACKOFF_MAX);
        self.retry_at = now.checked_add(delay);
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.retry_at.map(|retry_at| retry_at.saturating_duration_since(now))
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Default)]
struct LiveOracleLifecycle {
    session_last_used_at: Option<Instant>,
    respawn_backoff: RespawnBackoff,
}

impl LiveOracleLifecycle {
    fn next_wake_in(&self, has_backlog: bool, idle: Duration, now: Instant) -> Option<Duration> {
        if has_backlog {
            return self
                .respawn_backoff
                .remaining(now)
                .filter(|remaining| !remaining.is_zero())
                .or(Some(LIVE_ORACLE_RETRY_INTERVAL));
        }
        self.session_last_used_at
            .map(|last_used_at| scheduled_idle_wake_in(last_used_at, now, idle))
    }

    fn can_respawn(&self, now: Instant) -> bool {
        self.respawn_backoff.ready(now)
    }

    fn on_spawned(&mut self, now: Instant) {
        self.session_last_used_at = Some(now);
    }

    fn on_session_used(&mut self, now: Instant) {
        debug_assert!(self.session_last_used_at.is_some());
        self.session_last_used_at = Some(now);
    }

    fn idle_shutdown_due(&self, has_pending_work: bool, idle: Duration, now: Instant) -> bool {
        self.session_last_used_at.is_some_and(|last_used_at| {
            should_idle_shutdown(has_pending_work, last_used_at, now, idle)
        })
    }

    fn on_session_ended(&mut self) {
        self.session_last_used_at = None;
    }

    fn on_failure(&mut self, now: Instant) {
        self.on_session_ended();
        self.respawn_backoff.record_failure(now);
    }

    fn on_stable_batch(&mut self) {
        self.respawn_backoff.reset();
    }
}

/// One pass's changed paths, split by the checkout they belong to (#1010).
///
/// A linked worktree is a different source tree, so its edits can only be answered by a server
/// rooted there. Keeping the two halves separate all the way to the session is what stops a
/// linked checkout's paths from being handed to the main checkout's server, which would resolve
/// them against the wrong files.
pub(crate) struct LiveChangedSets<'a> {
    /// The base checkout's paths, or `None` when the pass has no reliable superset (a heal or
    /// bootstrap): live's scope is exactly "files the pass reindexed", never a whole-tree sweep.
    pub(crate) base: Option<&'a BTreeSet<String>>,
    /// What each visited linked checkout's overlay refresh changed, keyed by worktree id.
    pub(crate) overlays: &'a std::collections::BTreeMap<String, crate::watch::CheckoutReindex>,
}

/// Resident live-oracle state for one watcher: a [`CheckoutTail`] per checkout being served, each
/// holding a [`LiveBackendTail`] per live backend. Owned by the pass-worker closure (it must
/// outlive individual passes); the hook/CLI `maintenance_pass*` entry points pass no state, so the
/// live stage only ever runs from the resident watcher — a one-shot CLI pass must not spawn a
/// minutes-warming language server.
pub(crate) struct LiveOracleTail {
    /// Checkouts with live state. Created when one first has work, dropped when it has none left
    /// to remember. Entries are cheap (strings and counters); the expensive thing is the session,
    /// and that is what `max_checkouts` bounds.
    checkouts: Vec<CheckoutTail>,
}

/// One checkout's resident live state: where its servers are rooted, its per-backend sessions, and
/// when it last had work (which is what the `max_checkouts` cap ranks on).
struct CheckoutTail {
    /// `""` for the base checkout; otherwise the linked checkout's worktree id, which is both its
    /// root and the scope its overlay rows are keyed by.
    worktree_id: String,
    /// The directory this checkout's servers are rooted at — its own equivalent of `config.root`,
    /// NOT the checkout root, which differs whenever `config.root` is a repo subdir.
    root: std::path::PathBuf,
    backends: Vec<LiveBackendTail>,
    /// Which backend claims first within this checkout, rotated per served pass.
    first_claim: usize,
    /// When this checkout last received NEW changed paths — not merely when it still held a
    /// backlog. Stamping it for a backlog would give every waiting checkout the same instant on
    /// every pass, collapsing the ranking below onto its tie-breaks and pinning the slot to
    /// whichever checkout already held a session.
    last_worked_at: Instant,
    /// When this checkout last actually ran. `None` until it has. Breaks ties among checkouts
    /// that are only waiting, so the one kept out longest goes first instead of the lowest index.
    last_served_at: Option<Instant>,
    /// Whether NEW paths arrived for this checkout on the pass in flight. Transient.
    had_new_work: bool,
    /// Whether this pass's ranking gave this checkout a turn that amounted to something.
    /// Recomputed every pass.
    served: bool,
    /// Whether this pass gave this checkout a turn it could not take. Recomputed every pass; see
    /// [`Self::defer_turn`].
    deferred: bool,
    /// This checkout's effective configuration, resolved once by `absorb` when it had new paths
    /// and reused by [`Self::run`]. `None` for the base checkout, and on a pass where this
    /// checkout only carries a backlog (then `run` resolves it).
    pass_config: Option<Config>,
}

impl CheckoutTail {
    fn new(worktree_id: String, root: std::path::PathBuf, now: Instant) -> Self {
        Self {
            worktree_id,
            root,
            backends: LiveBackend::all().map(LiveBackendTail::new).collect(),
            first_claim: 0,
            last_worked_at: now,
            last_served_at: None,
            had_new_work: false,
            served: false,
            deferred: false,
            pass_config: None,
        }
    }

    /// Retain this pass's changed paths in the per-backend backlogs, whatever the cap decides.
    ///
    /// Retention and service are separate questions. The cap bounds resident SERVERS, so it may
    /// legitimately decline to run a checkout this pass — but declining to run it must never
    /// discard its edits, or an edit made in a checkout that happens to be outside the cap is lost
    /// with nothing to retry it. Each backend takes only the paths in its own languages, the same
    /// filter [`assemble_worklist`] applies, so a `.ts` edit never lands in the Rust backlog.
    fn retain_changed(&mut self, changed_paths: &BTreeSet<String>) {
        for backend in &mut self.backends {
            let known: HashSet<&String> = backend.backlog.iter().collect();
            let fresh: Vec<String> = changed_paths
                .iter()
                .filter(|path| backend.backend.claims_path(path) && !known.contains(path))
                .cloned()
                .collect();
            backend.backlog.extend(fresh);
        }
    }

    /// This checkout's share of one pass: every backend in rotating order, against a scope rooted
    /// at THIS checkout rather than at `config.root`.
    fn run(&mut self, db: &IndexDatabase, config: &Config, budget: &mut u64) -> bool {
        // The base checkout already IS `config`; a linked one needs the BRANCH's configuration
        // rooted at its own tree.
        //
        // Both halves matter. `for_linked_worktree_overlay` swaps in that checkout's own target
        // set — the same source the overlay refresh indexed it from — so a branch that ADDS a
        // target is not judged against the main checkout's bindings. Cloning the main config
        // instead would gate the added language out entirely: the backend's language check below
        // would fail, and that arm has already taken the backlog, so the linked edit would be
        // dropped outright. It also builds the corpus from the wrong bindings.
        //
        // Rooting it at this checkout is the other half: the branch config keeps the shared base
        // `root`, and the server must read this checkout's tree.
        // Resolved by `absorb` when this pass brought new paths (it needed the same answer to
        // decide admission); recomputed here only when the checkout is running a pure backlog.
        let linked_config = self
            .pass_config
            .take()
            .or_else(|| effective_config(config, &self.worktree_id, &self.root));
        let scope_config = linked_config.as_ref().unwrap_or(config);
        let scope = LiveScope { config: scope_config, corpus: std::cell::OnceCell::new() };
        let mut did_work = false;
        for index in claim_order(self.backends.len(), self.first_claim) {
            did_work |= self.backends[index].on_pass(db, scope_config, &scope, None, budget);
        }
        self.first_claim = self.first_claim.wrapping_add(1);
        did_work
    }

    /// Whether this checkout is holding anything worth keeping an entry for.
    fn is_idle(&self) -> bool {
        self.backends.iter().all(|backend| {
            backend.session.is_none()
                && backend.backlog.is_empty()
                && backend.unconfigured_paths.is_empty()
        })
    }

    fn has_session(&self) -> bool {
        self.backends.iter().any(|backend| backend.session.is_some())
    }

    /// Whether this checkout has anything a pass could actually act on.
    ///
    /// Parked (`unconfigured_paths`) entries do NOT count. They ride along with a real worklist and
    /// can only be re-skipped on their own, so a checkout holding nothing else would win a cap
    /// slot, run a pass that asks the server nothing, and schedule no wake — while the sibling it
    /// displaced is filtered out of the wake computation too. With the periodic sweep disabled that
    /// strands the sibling's work indefinitely. Such a checkout keeps its parked paths; it just
    /// does not compete for a slot.
    fn has_runnable_work(&self) -> bool {
        self.backends.iter().any(|backend| !backend.backlog.is_empty()) || self.has_session()
    }

    /// A checkout that got a turn it could not take: give up the turn and back the retry off,
    /// KEEPING its backlog. Distinct from cap eviction, which does not spend the turn.
    ///
    /// Marking it `deferred` is what keeps it in [`LiveOracleTail::next_wake_in`]. An unranked
    /// checkout is deliberately excluded from that — waking sooner cannot help a checkout a
    /// sibling is blocking — but this one HAD the slot, so its backoff is a real retry deadline
    /// and nothing else will necessarily schedule the pass that honours it.
    fn defer_turn(&mut self, now: Instant, reason: &'static str) {
        self.evict_sessions(reason);
        self.last_served_at = Some(now);
        self.deferred = true;
        for backend in &mut self.backends {
            backend.lifecycle.on_failure(now);
        }
    }

    /// Shut every resident server for this checkout down, keeping backlogs intact.
    fn evict_sessions(&mut self, reason: &'static str) {
        for backend in &mut self.backends {
            if let Some(session) = backend.session.take() {
                tracing::info!(
                    target: "rag_rat_core::watch",
                    tool = backend.tool_name(),
                    checkout = %self.root.display(),
                    reason,
                    "live oracle: shutting a resident server down"
                );
                backend.lifecycle.on_session_ended();
                session.shutdown();
            }
        }
    }
}

impl LiveOracleTail {
    pub(crate) fn new() -> Self {
        Self { checkouts: Vec::new() }
    }

    /// Delay until the watcher should run another pass without waiting for a filesystem event —
    /// the EARLIEST any backend of any checkout needs one, since a single pass services them all.
    pub(crate) fn next_wake_in(&self, config: &Config, now: Instant) -> Option<Duration> {
        if !config.oracle.live.enabled {
            return None;
        }
        let idle = Duration::from_secs(config.oracle.live.idle_shutdown_secs);
        self.checkouts
            .iter()
            // Checkouts that got a turn schedule a wake; ones that lost the ranking do not.
            // An unranked checkout's backlog cannot be drained by waking sooner — it is held out
            // precisely because a sibling owns the slot — so scheduling on its behalf would wake
            // the watcher on a cadence no pass can satisfy. It is picked up by a pass that happens
            // anyway: the served checkout's own idle-shutdown wake, or the periodic sweep.
            //
            // A DEFERRED checkout is the opposite case and must be included: it HAD its turn and
            // could not take it because the checkout was unreachable, so its backoff is a real
            // retry deadline. Excluding it meant that when every retained checkout was
            // unreachable, nothing scheduled a pass at all and the work stayed stranded even
            // after the checkouts came back.
            .filter(|checkout| checkout.served || checkout.deferred)
            .flat_map(|checkout| checkout.backends.iter())
            .filter_map(|backend| backend.next_wake_in(idle, now))
            .min()
    }

    /// One pass's live stage: every checkout with work, and every backend within it.
    ///
    /// `max_requests_per_pass` bounds the whole MAINTENANCE PASS, not each backend and not each
    /// checkout: the pass holds the repository write lock while it runs, so the cap is a lock-hold
    /// guarantee, and giving every (checkout, backend) pair its own copy would multiply the real
    /// bound by the size of the worktree fleet. Everything draws from ONE budget, and both
    /// rotations advance each pass so no checkout or language starves.
    ///
    /// `max_checkouts` bounds resident SERVERS, which is the memory cost. Checkouts are ranked by
    /// how recently they had work; those outside the cap have their sessions shut down but keep
    /// their backlogs, so their work is deferred rather than lost.
    pub(crate) fn on_pass(
        &mut self,
        db: &mut IndexDatabase,
        config: &Config,
        changed: &LiveChangedSets<'_>,
    ) -> anyhow::Result<()> {
        if !config.oracle.live.enabled {
            return Ok(());
        }
        let now = Instant::now();
        self.absorb(config, changed, now);
        // BEFORE ranking, not while serving: a checkout whose worktree is gone must not be given a
        // slot that then goes unused. Detecting it inside the serve loop meant the cap had already
        // been spent on it, the sibling that should have had the slot stayed unserved — and an
        // unserved checkout schedules no wake, so with the periodic sweep disabled its work was
        // stranded until an unrelated filesystem event.
        self.drop_dead_checkouts(config);

        let cap = config.oracle.live.max_checkouts.max(1);
        let mut budget = config.oracle.live.max_requests_per_pass;
        let mut scope_switched = false;
        let mut slots_used = 0usize;
        for checkout in &mut self.checkouts {
            checkout.served = false;
            checkout.deferred = false;
        }
        // Rank order decides WHO gets a turn; the cap counts turns that amounted to something.
        //
        // A slot is claimed by doing work, not by being admitted. Predicting up front whether a
        // checkout's paths are real work means matching, exactly, everything the run will check —
        // target membership, ignore rules, file existence, whether the paths even have oracle
        // candidates — and every approximation of that leaves a gap where a checkout takes the
        // sole slot, resolves nothing, and strands a sibling's genuine backlog. Observing the
        // outcome closes all of those at once, and lets admission stay a cheap filter.
        for index in self.rank() {
            if slots_used >= cap {
                break;
            }
            // Point the connection at THIS checkout's rows before its servers answer for them:
            // `run_live_oracle_pass` writes verdicts keyed to the active scope, and its guard
            // checks that scope against the tree the session reads.
            let worktree = (!self.checkouts[index].worktree_id.is_empty())
                .then(|| std::path::PathBuf::from(&self.checkouts[index].worktree_id));
            if let Err(err) = db.use_worktree_scope(&config.root, worktree.as_deref()) {
                tracing::warn!(
                    target: "rag_rat_core::watch",
                    checkout = %self.checkouts[index].root.display(),
                    error = %err,
                    "live oracle: could not scope the connection to this checkout; deferring it"
                );
                // Not a bare `continue`: that left the checkout neither served nor deferred, so
                // it scheduled no wake and its retained backlog was never retried once the
                // periodic sweep is off. A failed switch is the same kind of event as an
                // unreachable checkout — the turn is spent, the work is kept, the retry backs off.
                self.checkouts[index].defer_turn(now, "scope switch failed");
                continue;
            }
            scope_switched = true;
            // `use_worktree_scope` does NOT fail for a worktree that has gone away: by contract it
            // validates the path and silently falls back to the BASE scope. So a checkout removed
            // while it still held a backlog would land here scoped to base, get its pass rejected
            // by the root guard (the session reads a tree the rows no longer belong to), requeue
            // the work, and stay resident forever — occupying a slot the live checkouts need. The
            // scope we actually landed on is the authority on whether this checkout still exists.
            //
            // The base checkout's own scope id is NOT the empty string: `resolve_worktree_scope`
            // reports `worktree_id_of(config.root)` for it. `""` is this module's sentinel for
            // "the base checkout", so it has to be translated before the comparison — otherwise
            // the base checkout looks stale on every pass and is dropped.
            let expected_active = if self.checkouts[index].worktree_id.is_empty() {
                crate::index::worktree_id_of(&config.root)
            } else {
                self.checkouts[index].worktree_id.clone()
            };
            if db.active_worktree_id != expected_active {
                // Reaching here means the checkout is REGISTERED (the liveness pass above kept it)
                // but its tree could not be validated right now — a worktree on an unmounted or
                // briefly unreadable path. That is a deferral, NOT a removal: forgetting its
                // backlog would lose work nothing can rebuild, because a checkout that returns
                // with the same contents produces no changed paths for the overlay refresh to
                // report. Removal is `drop_dead_checkouts`'s call, made from the live set.
                //
                // Its turn IS spent, though, and the backoff slows the retry — otherwise an
                // unreachable checkout would win the slot every pass and never yield it.
                tracing::info!(
                    target: "rag_rat_core::watch",
                    checkout = %self.checkouts[index].root.display(),
                    "live oracle: this checkout is registered but not reachable right now; \
                     deferring its work"
                );
                self.checkouts[index].defer_turn(now, "checkout not reachable");
                continue;
            }
            // The work is already in the backlogs; the pass reads it from there.
            let did_work = self.checkouts[index].run(db, config, &mut budget);
            self.checkouts[index].last_served_at = Some(now);
            // Did that turn amount to anything? If not — its work evaporated at a gate, or the
            // changed files carried no oracle candidates — it must not hold a slot a sibling
            // could use THIS pass. The verdict comes from what the backends DID (a request
            // issued, or work still queued), not from the state left behind: a session spawned
            // only to discover there was nothing to ask is exactly the case that must not count.
            if did_work {
                self.checkouts[index].served = true;
                slots_used += 1;
            } else if self.checkouts[index].has_session() {
                // The turn spawned a server and then had nothing to ask it. Release it HERE, not
                // in the cleanup after the loop: the loop is about to give the freed slot to the
                // next checkout, which may spawn a server of its own, so deferring the eviction
                // would let a single pass hold one resident server per no-op checkout at once —
                // precisely the ceiling `max_checkouts` exists to impose.
                self.checkouts[index].evict_sessions("turn resolved nothing");
            }
        }
        // Sessions of checkouts that never got a turn: the cap is a bound on RESIDENT SERVERS, so
        // one that is not being served must not keep one alive. Backlogs are untouched.
        for checkout in &mut self.checkouts {
            if !checkout.served && checkout.has_session() {
                checkout.evict_sessions("checkout cap reached");
            }
            // The per-pass config cache must not outlive the pass that resolved it — a branch can
            // change its bindings between passes.
            checkout.pass_config = None;
        }
        // Forget checkouts holding nothing — no session, no backlog, no parked paths. A worktree
        // edited once must not keep an entry forever.
        self.checkouts.retain(|checkout| !checkout.is_idle());
        // Restore the base scope for the rest of the pass. Unlike everything else in this stage
        // this is NOT best-effort: the base reconcile, gc, and memory validation that follow all
        // assume base scope, and running them against a linked checkout's view would write
        // base-intended rows into an overlay. Failing the pass is recoverable — the next one
        // retries; proceeding on the wrong scope is not.
        if scope_switched {
            db.use_worktree_scope(&config.root, None).context(
                "live oracle: could not restore the base scope after the live stage; the rest of \
                 the maintenance pass would have run against a linked checkout's view",
            )?;
        }
        Ok(())
    }

    /// Forget linked checkouts that are no longer live siblings of the repo.
    ///
    /// Their backlogs name paths in a tree that is gone, so no pass can ever resolve them; left in
    /// place they keep competing for a cap slot. Only consulted when a LINKED checkout is actually
    /// held — the base checkout is never in the live set's linked half, and the common case (no
    /// linked live state) must not pay for a repo walk on every pass.
    fn drop_dead_checkouts(&mut self, config: &Config) {
        if !self.checkouts.iter().any(|checkout| !checkout.worktree_id.is_empty()) {
            return;
        }
        let (_, live) = crate::index::live_worktree_contexts(&config.root);
        self.checkouts.retain_mut(|checkout| {
            if checkout.worktree_id.is_empty() || live.contains(&checkout.worktree_id) {
                return true;
            }
            tracing::info!(
                target: "rag_rat_core::watch",
                checkout = %checkout.root.display(),
                "live oracle: this checkout is no longer a live sibling; dropping its state"
            );
            checkout.evict_sessions("checkout no longer a live sibling");
            false
        });
    }

    /// Fold this pass's changed sets into per-checkout state: create entries for checkouts that
    /// now have work, and retain their paths in the per-backend backlogs.
    fn absorb(&mut self, config: &Config, changed: &LiveChangedSets<'_>, now: Instant) {
        let mut pass_changed = std::collections::BTreeMap::new();
        if let Some(base) = changed.base {
            pass_changed.insert(String::new(), base.clone());
        }
        for (worktree_id, entry) in changed.overlays {
            // A PARTIAL report is used, not discarded. Its coverage flag says the list may be
            // MISSING paths (the checkout's working-tree status read failed, so dirty, untracked,
            // and deleted files never became candidates) — it does not say the listed paths are
            // doubtful. The committed tree-diff half still ran, and every path here had its
            // overlay row written, so the list is SOUND and merely incomplete.
            //
            // That distinction is what this stage needs. A best-effort freshness patch loses
            // nothing by refreshing a subset: doing some of the work is the same soundness
            // direction as the backlog, and the omitted half surfaces on the next refresh (a
            // partial pass clears the overlay basis, so one is guaranteed). Dropping the entry
            // instead threw away work that was known stale AND known which — and, since a
            // non-empty backlog is what schedules the next wake, left nothing to schedule one.
            //
            // Do NOT widen a partial report to a whole-checkout sweep: unknown is not the same as
            // everything, and that sweep is the batch pass's job, not something to run under the
            // maintenance pass's write lock.
            if entry.paths.is_empty() {
                continue;
            }
            pass_changed.insert(
                worktree_id.clone(),
                entry.paths.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
            );
        }
        // Admit only checkouts a pass would actually DO something for — judged by that checkout's
        // OWN configuration, which is also the configuration its backends will run under. See
        // `would_serve`: "nonempty" and "the extension looks live" are both too weak, and each
        // false admission is a cap slot spent on a checkout that resolves nothing while a sibling
        // with real work stays unserved.
        //
        // The effective config is resolved ONCE here and handed to the run below, so the branch's
        // `rag-rat.toml` is read at most once per checkout per pass.
        let mut admitted = Vec::new();
        for (worktree_id, paths) in std::mem::take(&mut pass_changed) {
            // An already-held checkout keeps the root it was created with: re-deriving it every
            // pass would repeat the repo discovery, and a transient failure would drop a checkout
            // that is perfectly fine.
            let existing_root = self
                .checkouts
                .iter()
                .find(|checkout| checkout.worktree_id == worktree_id)
                .map(|checkout| checkout.root.clone());
            let Some(root) = existing_root.or_else(|| checkout_root(config, &worktree_id)) else {
                continue;
            };
            let effective = effective_config(config, &worktree_id, &root);
            if !would_serve(effective.as_ref().unwrap_or(config), &paths) {
                continue;
            }
            admitted.push((worktree_id, paths, root, effective));
        }
        for (worktree_id, paths, root, effective) in admitted {
            if !self.checkouts.iter().any(|checkout| checkout.worktree_id == worktree_id) {
                self.checkouts.push(CheckoutTail::new(worktree_id.clone(), root, now));
            }
            if let Some(checkout) =
                self.checkouts.iter_mut().find(|checkout| checkout.worktree_id == worktree_id)
            {
                checkout.pass_config = effective;
            }
            pass_changed.insert(worktree_id, paths);
        }
        // ...and RETAIN its paths before the cap gets a say. The cap decides which checkouts are
        // SERVED, never which are remembered: an edit in a checkout that happens to fall outside
        // it must wait, not vanish. Doing this after the cap check (or only inside the per-backend
        // pass, which unserved checkouts never reach) silently dropped those edits, leaving
        // nothing to retry them.
        for checkout in &mut self.checkouts {
            let Some(paths) = pass_changed.get(&checkout.worktree_id) else {
                checkout.had_new_work = false;
                continue;
            };
            checkout.retain_changed(paths);
            checkout.had_new_work = true;
            checkout.last_worked_at = now;
        }
    }

    /// Mark which checkouts run this pass, and shut down the servers of those that do not.
    ///
    /// Ranking, in order:
    ///
    /// 1. checkouts that received NEW paths this pass, so the one being edited right now keeps its
    ///    warm server;
    /// 2. among those, the most recently worked;
    /// 3. then the one kept waiting longest — never-served first.
    ///
    /// Rule 3 is what stops a waiting checkout from starving. Ranking on "still has a backlog"
    /// instead would stamp every waiting checkout with the same instant on every pass, collapsing
    /// the order onto its tie-breaks and handing the slot to whichever checkout already held a
    /// session — permanently, since holding a session is exactly what such a tie-break rewards.
    ///
    /// A checkout outside the cap while another is edited continuously is still not served until
    /// that editing pauses. It does not spin the watcher meanwhile: an unadmitted checkout
    /// schedules no wake (see [`Self::next_wake_in`]), so its backlog waits for a pass that
    /// happens anyway — the served checkout's own idle-shutdown wake, or the periodic sweep, the
    /// same backstop the overlay quiet window relies on.
    fn rank(&self) -> Vec<usize> {
        // Only checkouts with something to run compete. A checkout holding nothing but parked
        // paths would otherwise spend a turn on a pass that asks its server nothing.
        let mut ranked: Vec<usize> = (0..self.checkouts.len())
            .filter(|&index| self.checkouts[index].has_runnable_work())
            .collect();
        ranked.sort_by(|&left, &right| {
            let (a, b) = (&self.checkouts[left], &self.checkouts[right]);
            // New work first — the checkout being edited right now.
            b.had_new_work
                .cmp(&a.had_new_work)
                .then_with(|| {
                    if a.had_new_work {
                        // Both are being edited: the more recent one leads.
                        b.last_worked_at.cmp(&a.last_worked_at)
                    } else {
                        // Both are only WAITING, so service age decides and nothing else may
                        // outrank it. Comparing historical work recency first would let a
                        // checkout that was edited more recently — and already served for it —
                        // keep beating one that has never run, which is the starvation this
                        // ordering exists to prevent. `None` (never served) sorts first.
                        a.last_served_at.cmp(&b.last_served_at)
                    }
                })
                .then_with(|| a.last_served_at.cmp(&b.last_served_at))
                .then_with(|| left.cmp(&right))
        });
        ranked
    }
}

#[cfg(test)]
impl LiveOracleTail {
    /// `(worktree id, server root)` for every checkout this tail is holding. The shared-database
    /// worktree regressions live where the git fixtures are, so they reach the state through here.
    pub(crate) fn checkout_roots(&self) -> Vec<(String, std::path::PathBuf)> {
        self.checkouts
            .iter()
            .map(|checkout| (checkout.worktree_id.clone(), checkout.root.clone()))
            .collect()
    }

    /// The worktree ids the last pass's cap admitted.
    pub(crate) fn served_checkouts(&self) -> Vec<String> {
        self.checkouts
            .iter()
            .filter(|checkout| checkout.served)
            .map(|checkout| checkout.worktree_id.clone())
            .collect()
    }

    /// Every path a checkout is still holding, across its backends.
    pub(crate) fn backlog_for(&self, worktree_id: &str) -> Vec<String> {
        let mut paths: Vec<String> = self
            .checkouts
            .iter()
            .filter(|checkout| checkout.worktree_id == worktree_id)
            .flat_map(|checkout| checkout.backends.iter())
            .flat_map(|backend| backend.backlog.iter().cloned())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// A checkout's effective configuration: the BRANCH's target bindings, rooted at its own tree.
/// `None` for the base checkout, which `config` already describes.
fn effective_config(config: &Config, worktree_id: &str, root: &std::path::Path) -> Option<Config> {
    (!worktree_id.is_empty()).then(|| Config {
        root: root.to_path_buf(),
        ..config.for_linked_worktree_overlay(std::path::Path::new(worktree_id))
    })
}

/// Whether a pass would actually DO anything with `paths` in a checkout configured by `cfg`: does
/// SOME PATH belong to a target this checkout indexes, in a language a live backend resolves?
///
/// Asked per path through `target_for_path` — the same membership predicate indexing itself uses,
/// so directories, includes, and excludes all count. Weaker approximations keep producing the same
/// bug in narrower forms, because each gap between this predicate and the real one is a cap slot
/// spent on a checkout that resolves nothing while a sibling's real work waits:
///
/// - "the changed set is nonempty" admits a checkout that only changed markdown;
/// - "some live backend claims the extension" admits one whose branch dropped that language
///   entirely, since the pruned sources still end in `.rs`;
/// - "some target is Rust AND some path ends in `.rs`" admits one that dropped the target those
///   particular paths lived under, because the two halves are independent.
///
/// Matching each path against its own target closes the class rather than the instance.
fn would_serve(cfg: &Config, paths: &BTreeSet<String>) -> bool {
    paths.iter().any(|path| {
        crate::index::target_for_path(cfg, std::path::Path::new(path)).is_some_and(
            |(language, _kind)| {
                LiveBackend::all().any(|backend| backend.resolves_language(language))
            },
        )
    })
}

/// Where a checkout's servers are rooted: its own equivalent of `config.root`.
///
/// For a linked worktree that is NOT the checkout root — with a subdir-rooted config the two are
/// `<linked>/crate` and `<linked>`, and a server rooted at the latter would initialize a workspace
/// that does not contain the indexed sources. Same derivation the overlay uses to read that
/// checkout's bytes, so the session and the rows it patches agree.
fn checkout_root(config: &Config, worktree_id: &str) -> Option<std::path::PathBuf> {
    if worktree_id.is_empty() {
        return Some(config.root.clone());
    }
    match crate::index::linked_source_root(&config.root, std::path::Path::new(worktree_id)) {
        Ok(root) => Some(root),
        Err(err) => {
            tracing::debug!(
                target: "rag_rat_core::watch",
                worktree = worktree_id,
                error = %err,
                "live oracle: could not resolve a linked checkout's source root; skipping it"
            );
            None
        },
    }
}

/// The live stage's checkout scope, whose corpus is built on FIRST USE.
///
/// [`crate::index::corpus::ConfiguredCorpus`] compiles the ignore matcher, which discovers nested
/// `.gitignore` files along every configured target tree. That is real work, and the maintenance
/// pass holds the repository write lock while it happens — but most passes have no live work at all
/// (no changed path in a live language, no backlog), and those must not pay for it. Building it
/// eagerly made every watcher event, however small, walk the target trees before deciding there was
/// nothing to do.
///
/// Shared by every backend in the pass, so a mixed-language checkout compiles it once rather than
/// once per resident server.
struct LiveScope<'a> {
    config: &'a Config,
    corpus: std::cell::OnceCell<crate::index::corpus::ConfiguredCorpus<'a>>,
}

impl LiveScope<'_> {
    fn resolve(&self) -> rag_rat_oracle::CheckoutScope<'_> {
        let corpus =
            self.corpus.get_or_init(|| crate::index::corpus::ConfiguredCorpus::new(self.config));
        rag_rat_oracle::CheckoutScope::resolve(&self.config.root, corpus)
    }
}

/// Backend indices for one pass, starting at `first` and wrapping — the rotation that keeps the
/// shared request budget fair across passes.
fn claim_order(count: usize, first: usize) -> impl Iterator<Item = usize> {
    (0..count).map(move |offset| (first.wrapping_add(offset)) % count.max(1))
}

/// One live backend's resident state: its LSP session, deferred-path backlog, and respawn/idle
/// lifecycle. Independent of every other backend's.
struct LiveBackendTail {
    backend: LiveBackend,
    session: Option<LiveOracleSession>,
    backlog: Vec<String>,
    /// Paths a pass skipped because the session could not configure their files, held HERE rather
    /// than in `backlog`: a non-empty backlog is what schedules the next pass, and these can only
    /// be skipped again until the checkout's project layout changes. They ride along with the next
    /// worklist that has real work in it — see [`assemble_worklist`].
    ///
    /// Bounded by being a SET fed only from worklists this backend actually ran: it holds at most
    /// one entry per distinct file of this backend's languages the watcher has seen change, and a
    /// path leaves again as soon as a pass carries it without skipping it. Re-editing the same
    /// unconfigurable file does not grow it.
    unconfigured_paths: BTreeSet<String>,
    lifecycle: LiveOracleLifecycle,
    /// Whether this backend's unmet checkout prerequisite has already been reported. The block is
    /// permanent until the checkout changes, so it is worth saying — once, not on every retry.
    prerequisite_reported: bool,
    /// Consecutive passes this backend has spent warming without ever resolving anything, and
    /// whether that has been reported. See [`WARMING_PASSES_BEFORE_REPORT`].
    warming_passes: u32,
    warming_reported: bool,
    /// Whether a pass that asked the server nothing because it could configure none of its files
    /// has already been reported. Cleared as soon as a pass issues a request. See
    /// [`LiveBackendTail::note_unconfigured`].
    unconfigured_reported: bool,
}

/// How many consecutive warming passes go by before the watcher says so. A cold language server
/// legitimately warms for a pass or two; a server that never becomes ready is a real problem the
/// operator cannot otherwise see, because the safe behaviour — never ask a warming server — is
/// also a SILENT one.
///
/// The causes are open-ended (a language server that cannot resolve a compiler for the project, a
/// broken toolchain install, a project too large to load inside the retry window), so this reports
/// the observed state rather than trying to predict any particular cause.
const WARMING_PASSES_BEFORE_REPORT: u32 = 5;

impl LiveBackendTail {
    fn tool_name(&self) -> &'static str {
        self.backend.tool.as_db_str()
    }

    fn new(backend: LiveBackend) -> Self {
        Self {
            backend,
            session: None,
            backlog: Vec::new(),
            unconfigured_paths: BTreeSet::new(),
            lifecycle: LiveOracleLifecycle::default(),
            prerequisite_reported: false,
            warming_passes: 0,
            warming_reported: false,
            unconfigured_reported: false,
        }
    }

    /// Delay until this backend needs another pass. A backlog takes priority; otherwise a
    /// resident session schedules its own idle shutdown.
    fn next_wake_in(&self, idle: Duration, now: Instant) -> Option<Duration> {
        self.lifecycle.next_wake_in(!self.backlog.is_empty(), idle, now)
    }

    /// Track consecutive warming passes and report a server that never becomes ready.
    ///
    /// Refusing to ask a warming server is the correct behaviour, but on its own it is
    /// indistinguishable from working: the backlog just rides forever. Say so once, and reset as
    /// soon as the backend gets anywhere, so a normally-warming server stays quiet.
    fn note_warming(&mut self, status: &str) {
        if status != "Warming" {
            self.warming_passes = 0;
            self.warming_reported = false;
            return;
        }
        self.warming_passes = self.warming_passes.saturating_add(1);
        if self.warming_passes >= WARMING_PASSES_BEFORE_REPORT && !self.warming_reported {
            self.warming_reported = true;
            tracing::warn!(
                target: "rag_rat_core::watch",
                tool = self.backend.tool.as_db_str(),
                passes = self.warming_passes,
                "live oracle: the language server has not reported a completed project load — \
                 it resolves nothing until it does. Check that it can load this project (for \
                 TypeScript, that a `typescript` package resolves for the tsconfig project)."
            );
        }
    }

    /// Report a pass that issued no requests at all while skipping candidates whose files the
    /// session cannot configure.
    ///
    /// Skipping such a file is the correct answer — the server would otherwise answer it with
    /// fallback flags, which resolve a call into another translation unit to the callee's header
    /// declaration, and that wrong answer would be persisted as a real verdict. But the skip is
    /// deliberately not deferred, so a pass made entirely of them writes no rows AND leaves no
    /// backlog: the two things the per-pass log keys on. Say it once, and reset as soon as a pass
    /// issues a request, so a checkout the session can configure stays quiet.
    fn note_unconfigured(&mut self, report: &LivePassReport) {
        // One request is enough to prove this session configures something in this checkout.
        if report.requests_used > 0 {
            self.unconfigured_reported = false;
            return;
        }
        if report.skipped_unconfigured == 0 || self.unconfigured_reported {
            return;
        }
        self.unconfigured_reported = true;
        if report.database_unreadable {
            // The marker's name comes from the backend's own declaration, like every other
            // message that names one. Spelling it out here would let a second checkout-scoped
            // backend send an operator to a file it does not use, with nothing to catch it.
            let marker = self.backend.marker_name_hint();
            tracing::warn!(
                target: "rag_rat_core::watch",
                tool = self.backend.tool.as_db_str(),
                skipped = report.skipped_unconfigured,
                "live oracle: this pass sent the server no requests and skipped candidates because \
                 this checkout's sole compilation database could not be used by this reader. \
                 Inspect {marker}: it may be unreadable, contain unsupported syntax (such as \
                 clangd's YAML forms), or contain an unsupported entry key."
            );
            return;
        }
        // The three causes have DIFFERENT remedies, and the generic one sends an operator whose
        // database simply does not cover their sources after the wrong problem entirely.
        //
        // A checkout that ALREADY governs nothing at spawn never reaches here — it blocks on the
        // prerequisite instead, which carries this same remedy (`prerequisite_blocked_with`). What
        // this branch covers is the decay: a checkout that warmed on several databases and has
        // since been reduced to one that governs nothing, where the session is still alive because
        // the pinned database did not change (both layouts pin none).
        if report.database_governs_nothing {
            tracing::warn!(
                target: "rag_rat_core::watch",
                tool = self.backend.tool.as_db_str(),
                skipped = report.skipped_unconfigured,
                "live oracle: this checkout's compilation database names no file this checkout \
                 indexes, so it is not pinned for the server — forcing it on first-party sources \
                 would analyse them under another project's defines and include paths, which \
                 resolves calls to the wrong definition. Regenerate the database so it covers the \
                 indexed sources, or bind the tree it does describe in `[target_bindings]` if it \
                 is meant to be indexed."
            );
            return;
        }
        // TERMINAL branch: it runs for every layout the branches above did not claim, and that
        // set is defined as the complement of theirs — new verdict/governs combinations join it
        // silently, so it must not assert a diagnosis. A sole database whose entries parse but
        // carry a non-string `file` lands here today (#1255), and telling that operator to
        // consolidate the databases they do not have sends them after the wrong problem
        // entirely. Adding a branch would narrow the set without closing it. So: state what was
        // observed, offer the causes, and make each remedy conditional on its own cause.
        tracing::warn!(
            target: "rag_rat_core::watch",
            tool = self.backend.tool.as_db_str(),
            skipped = report.skipped_unconfigured,
            "live oracle: this pass sent the server no requests and skipped candidates because \
             the session cannot configure their files. The layout facts collected do not identify \
             which cause, so both are worth checking. A compilation database is pinned for the \
             server (`--compile-commands-dir`) only when the checkout holds exactly one: if it \
             holds several, the server has to find each file's database itself and skips a file \
             it finds none for — leave a single compilation database, or put each file's database \
             in one of its ancestor directories or that directory's `build/`. If it holds exactly \
             one, that database may parse while still not proving it covers these files — an \
             entry whose `file` is not a string is the usual shape — so regenerate it with string \
             `file` fields naming the indexed sources."
        );
    }

    /// Fold one pass's unconfigurable skips into the retained set.
    ///
    /// A path this pass CARRIED and did not skip is either configurable now or carries no
    /// candidates at all; either way it stops being retained. That is what makes the ride-along in
    /// [`assemble_worklist`] settle: parked paths join a worklist that already has real work, and
    /// each pass either resolves them or puts them back.
    fn retain_unconfigured(&mut self, worklist: &[String], report: &LivePassReport) {
        for path in worklist {
            self.unconfigured_paths.remove(path);
        }
        self.unconfigured_paths.extend(report.skipped_unconfigured_paths.iter().cloned());
    }

    /// This backend's share of one pass: resolve pending work, or shut an otherwise-workless
    /// session down once idle. Never returns an error — a failure is logged and the work rides
    /// the next pass.
    ///
    /// Returns whether the turn AMOUNTED to anything: it issued at least one request, or it still
    /// holds work to retry. Holding a session is deliberately not enough — a changed file with no
    /// call candidates spawns a server, asks it nothing, and finishes with an empty backlog, and
    /// that must not consume a checkout slot a sibling could use (see `CheckoutTail::run`).
    fn on_pass(
        &mut self,
        db: &IndexDatabase,
        config: &Config,
        scope: &LiveScope<'_>,
        changed_paths: Option<&BTreeSet<String>>,
        budget: &mut u64,
    ) -> bool {
        let live_cfg = &config.oracle.live;
        let now = Instant::now();
        let started_at_ms = now_ms();

        // The worklist: backlog first (older edits wait longest), then this pass's changed paths
        // in this backend's language, deduped. `changed_paths` is `None` on a heal/bootstrap — no
        // reliable superset, so only the backlog rides.
        let worklist = assemble_worklist(
            std::mem::take(&mut self.backlog),
            &mut self.unconfigured_paths,
            changed_paths,
            &self.backend,
        );
        let idle_shutdown_due = self.lifecycle.idle_shutdown_due(
            !worklist.is_empty(),
            Duration::from_secs(live_cfg.idle_shutdown_secs),
            now,
        );
        if worklist.is_empty() {
            // Pending work always wins over idle shutdown: a short idle timeout must not kill a
            // warming server immediately before its retained backlog retries.
            if idle_shutdown_due && let Some(session) = self.session.take() {
                tracing::debug!(target: "rag_rat_core::watch", "live oracle: idle shutdown");
                self.lifecycle.on_session_ended();
                session.shutdown();
            }
            return false;
        }
        // An earlier backend spent the pass's whole request allowance. Keep this backend's work
        // (a spawn + a zero-budget pass would only defer it again, after paying a language-server
        // warm-up) and let the rotation give it first claim on a later pass.
        if *budget == 0 {
            self.backlog = worklist;
            return true;
        }
        // One of this backend's languages must be indexed in the checkout (clangd serves two).
        if !config.targets.iter().any(|target| self.backend.resolves_language(target.language)) {
            return false;
        }
        // Past every cheap gate, so this pass really is going to talk to a server: NOW the corpus
        // is worth compiling.
        let scope = &scope.resolve();

        // Spawn the session lazily on the first eligible pass. A decline leaves the work in the
        // backlog for a later pass — the same degrade-quietly UX as a missing embedding model.
        if self.session.is_none() && !spawn_suppressed() {
            if !self.lifecycle.can_respawn(now) {
                self.backlog = worklist;
                return true;
            }
            match LiveOracleSession::spawn(self.backend.tool, scope) {
                Ok(session) => {
                    self.session = Some(session);
                    self.lifecycle.on_spawned(now);
                },
                // A prerequisite block is PERMANENT until the checkout changes, so retrying it
                // silently would leave an operator with a live oracle that never runs and no
                // reason why. Say it once, then fall through to the ordinary backoff (which is
                // the right cadence for something that cannot fix itself).
                Err(rag_rat_oracle::LiveSpawnBlocked::Prerequisite(hint)) => {
                    if !self.prerequisite_reported {
                        self.prerequisite_reported = true;
                        tracing::warn!(
                            target: "rag_rat_core::watch",
                            tool = self.backend.tool.as_db_str(),
                            "live oracle blocked: {hint}"
                        );
                    }
                },
                Err(rag_rat_oracle::LiveSpawnBlocked::Unavailable) => {},
            }
        }
        let Some(session) = &mut self.session else {
            self.backlog = worklist;
            self.lifecycle.on_failure(Instant::now());
            return true;
        };
        // A session exists, so whatever blocked earlier is resolved; report it again if it returns.
        self.prerequisite_reported = false;

        let result = db.run_live_oracle_pass(session, scope, &worklist, *budget, started_at_ms);
        // Count idleness from completion: a request batch longer than the idle window must not
        // force an immediate shutdown and cold respawn.
        self.lifecycle.on_session_used(Instant::now());
        match result {
            Ok(report) => {
                // Charge what was actually spent against the pass-wide allowance, so the
                // backends that run after this one see a real remainder.
                *budget = budget.saturating_sub(report.requests_used);
                self.backlog = report.unfinished_paths.clone();
                self.retain_unconfigured(&worklist, &report);
                // An aborted pass means the server died or wedged mid-resolution, or the checkout
                // moved out from under the session: drop it so the next pass respawns a clean one
                // instead of reusing a broken transport or a stale argv (the aborted files are
                // already requeued in `unfinished_paths`).
                if report.abort.is_some()
                    && let Some(_aborted_session) = self.session.take()
                {
                    // Let the binding hard-kill on Drop; graceful shutdown would attempt another
                    // bounded request against the same wedged transport.
                    tracing::warn!(
                        target: "rag_rat_core::watch",
                        status = %report.status,
                        "live oracle: server aborted; session dropped, respawn after backoff"
                    );
                    self.lifecycle.on_failure(Instant::now());
                } else if report.status != "Warming" {
                    // A completed request batch proves the replacement session is stable enough
                    // to end an earlier crash/spawn-failure streak. Warm-up alone does not.
                    self.lifecycle.on_stable_batch();
                }
                self.note_warming(&report.status);
                self.note_unconfigured(&report);
                // A pass whose candidates were all skipped as unconfigured writes no rows and
                // defers nothing (those skips are not retried), so it has to be admitted to the
                // log on its own count or the pass leaves no trace at all.
                if report.rows_written > 0
                    || !report.unfinished_paths.is_empty()
                    || report.skipped_unconfigured > 0
                {
                    tracing::info!(
                        target: "rag_rat_core::watch",
                        rows_written = report.rows_written,
                        upgraded = report.upgraded,
                        confirmed = report.confirmed,
                        contradicted = report.contradicted,
                        requests = report.requests_used,
                        deferred = report.unfinished_paths.len(),
                        skipped_unconfigured = report.skipped_unconfigured,
                        refinements_invalidated = report.refinements_invalidated,
                        status = %report.status,
                        "live oracle pass"
                    );
                }
                // Did this turn amount to anything? A request issued, or work still queued (a
                // warming server defers its whole worklist, so it counts). A session that was
                // spawned only to find the changed file has no call candidates does NOT — it
                // would otherwise hold the checkout slot until its idle shutdown.
                report.requests_used > 0 || !self.backlog.is_empty()
            },
            Err(err) => {
                // A DB-side failure (the only `Err`): drop the session so the next pass
                // respawns fresh, keep the whole worklist riding, and never fail the pass.
                if let Some(session) = self.session.take() {
                    session.shutdown();
                }
                self.backlog = worklist;
                self.lifecycle.on_failure(Instant::now());
                tracing::warn!(
                    target: "rag_rat_core::watch",
                    error = %err,
                    "live oracle pass failed; worklist deferred to the next pass"
                );
                true
            },
        }
    }
}

fn idle_wake_in(last_used_at: Instant, now: Instant, idle: Duration) -> Duration {
    idle.saturating_sub(now.saturating_duration_since(last_used_at))
}

fn scheduled_idle_wake_in(last_used_at: Instant, now: Instant, idle: Duration) -> Duration {
    let remaining = idle_wake_in(last_used_at, now, idle);
    // If a pass failed before reaching the tail, do not redispatch an overdue deadline in a tight
    // loop. A serviced idle wake removes the session and therefore returns no next wake.
    if remaining.is_zero() { LIVE_ORACLE_RETRY_INTERVAL } else { remaining }
}

fn should_idle_shutdown(
    has_pending_work: bool,
    last_used_at: Instant,
    now: Instant,
    idle: Duration,
) -> bool {
    !has_pending_work && now.saturating_duration_since(last_used_at) >= idle
}

/// The pass's worklist for one backend: backlog paths first (oldest edits wait longest), then the
/// changed paths this backend's language claims, deduped, order-preserving. `None` changed paths
/// (a heal/bootstrap pass) contributes nothing — only the backlog rides.
///
/// The language filter is what keeps backends disjoint: a changed `.ts` file must never reach the
/// Rust session, which would open it under the wrong `languageId` and spend budget on a file its
/// server cannot resolve.
fn assemble_worklist(
    backlog: Vec<String>,
    parked: &mut BTreeSet<String>,
    changed_paths: Option<&BTreeSet<String>>,
    backend: &LiveBackend,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut worklist = Vec::new();
    for path in backlog {
        if seen.insert(path.clone()) {
            worklist.push(path);
        }
    }
    if let Some(paths) = changed_paths {
        for path in paths.iter().filter(|path| backend.claims_path(path)) {
            if seen.insert(path.clone()) {
                worklist.push(path.clone());
            }
        }
    }
    // Paths this backend could not configure ride ALONG with real work — never on their own.
    //
    // Retrying them cannot help until the checkout's project layout changes, so they must not be
    // what SCHEDULES a pass: a non-empty backlog is exactly what makes the watcher wake, and
    // parking them there would spin it forever on work every pass can only re-skip. But a pass
    // that is happening anyway re-answers "can I configure this?" for free — the session's layout
    // is already resolved, and an unconfigurable path is skipped before the request budget is
    // touched. So an operator who does what the unconfigured warning asked and consolidates the
    // checkout's compilation databases gets those sources resolved by the next pass, instead of
    // only if one happens to abort on the layout change.
    //
    // What this deliberately does NOT do is let a `compile_commands.json` edit trigger a pass by
    // itself: that edit contributes no path in this backend's languages, so the worklist stays
    // empty and the parked paths wait for the next source edit. Closing that needs a layout signal
    // independent of the worklist — see #996.
    if worklist.is_empty() {
        return worklist;
    }
    // Drained, not copied: whatever the pass still cannot configure comes back through
    // `retain_unconfigured`, and anything it resolves is simply gone.
    for path in std::mem::take(parked) {
        if seen.insert(path.clone()) {
            worklist.push(path);
        }
    }
    worklist
}

#[cfg(test)]
mod tests {
    use rag_rat_oracle::{CheckoutScope, IndexedCorpus, LivePassAbort};

    use super::*;

    fn backend(tool: rag_rat_oracle::OracleTool) -> LiveBackend {
        LiveBackend::for_tool(tool).expect("a live backend")
    }

    /// A tail holding one checkout, for the tests that exercise BACKEND-level behaviour (backlog,
    /// backoff, wake scheduling). Those properties are per backend within a checkout and are
    /// unchanged by the per-checkout split.
    fn single_checkout_tail(now: Instant) -> LiveOracleTail {
        LiveOracleTail {
            checkouts: vec![CheckoutTail::new(
                String::new(),
                std::path::PathBuf::from("/repo"),
                now,
            )],
        }
    }

    fn backends_of(tail: &mut LiveOracleTail) -> &mut Vec<LiveBackendTail> {
        &mut tail.checkouts[0].backends
    }

    #[test]
    fn worklist_dedupes_backlog_against_changed_and_filters_other_languages() {
        let rust = backend(rag_rat_oracle::OracleTool::RaLsp);
        let backlog = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let changed = BTreeSet::from([
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
            "Cargo.toml".to_string(),
        ]);
        let worklist = assemble_worklist(backlog, &mut BTreeSet::new(), Some(&changed), &rust);
        // Backlog order first, then new changed paths; duplicates collapse; non-Rust dropped.
        assert_eq!(worklist, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    }

    #[test]
    fn worklist_without_changed_set_rides_backlog_only() {
        let rust = backend(rag_rat_oracle::OracleTool::RaLsp);
        let backlog = vec!["src/a.rs".to_string()];
        // A heal/bootstrap pass (None) contributes no paths.
        assert_eq!(assemble_worklist(backlog, &mut BTreeSet::new(), None, &rust), vec!["src/a.rs"]);
        assert!(assemble_worklist(Vec::new(), &mut BTreeSet::new(), None, &rust).is_empty());
    }

    #[test]
    fn each_backend_claims_only_its_own_languages_changed_paths() {
        // One changed set, several backends: a `.ts` file reaching the Rust session would be
        // opened under the wrong languageId and burn budget on a file that server cannot
        // resolve, and vice versa.
        let changed = BTreeSet::from([
            "src/a.rs".to_string(),
            "src/b.ts".to_string(),
            "src/c.tsx".to_string(),
            "README.md".to_string(),
        ]);
        assert_eq!(
            assemble_worklist(
                Vec::new(),
                &mut BTreeSet::new(),
                Some(&changed),
                &backend(rag_rat_oracle::OracleTool::RaLsp)
            ),
            vec!["src/a.rs"],
        );
        assert_eq!(
            assemble_worklist(
                Vec::new(),
                &mut BTreeSet::new(),
                Some(&changed),
                &backend(rag_rat_oracle::OracleTool::TsLsp)
            ),
            vec!["src/b.ts", "src/c.tsx"],
        );
    }

    #[test]
    fn the_tail_wakes_for_the_earliest_backend_that_needs_one() {
        // A single pass services every backend, so the tail's deadline is the minimum across
        // them — a backend with a backlog must not wait for another backend's longer idle timer.
        let now = Instant::now();
        let mut tail = single_checkout_tail(now);
        assert!(
            backends_of(&mut tail).len() >= 2,
            "the multi-backend case must actually be exercised"
        );
        let idle = Duration::from_secs(600);
        // One backend holds a backlog (retry cadence); another holds an idle session.
        backends_of(&mut tail)[0].backlog.push("src/a.rs".to_string());
        backends_of(&mut tail)[1].lifecycle.on_spawned(now);
        let earliest = backends_of(&mut tail)
            .iter()
            .filter_map(|backend| backend.next_wake_in(idle, now))
            .min()
            .expect("at least one backend schedules a wake");
        assert_eq!(earliest, LIVE_ORACLE_RETRY_INTERVAL, "the backlog's retry wins over idle");
    }

    #[test]
    fn the_claim_order_rotates_so_no_backend_starves_the_shared_budget() {
        // `max_requests_per_pass` bounds the whole pass, so the backends share one allowance. If
        // the order were fixed, a language whose change set always exhausts it would keep every
        // other language's backlog permanently unserviced.
        let order = |first| claim_order(3, first).collect::<Vec<_>>();
        assert_eq!(order(0), vec![0, 1, 2]);
        assert_eq!(order(1), vec![1, 2, 0]);
        assert_eq!(order(2), vec![2, 0, 1]);
        // Every backend is still visited exactly once per pass, whatever the rotation.
        assert_eq!(order(7).len(), 3);
        assert_eq!(order(7).iter().collect::<HashSet<_>>().len(), 3);
        // A wrapped counter must not panic or skip anyone.
        assert_eq!(claim_order(2, usize::MAX).collect::<Vec<_>>(), vec![1, 0]);
        assert_eq!(claim_order(0, 5).count(), 0, "no backends is not a division by zero");
    }

    #[test]
    fn a_server_that_never_warms_is_reported_once_then_stays_quiet() {
        // Refusing to ask a warming server is correct but SILENT — on its own it is
        // indistinguishable from the backend working, and the backlog just rides forever. The
        // watcher has to say so, once, and stop as soon as the backend gets anywhere.
        let mut tail =
            LiveBackendTail::new(LiveBackend::for_tool(rag_rat_oracle::OracleTool::TsLsp).unwrap());
        for _ in 0..WARMING_PASSES_BEFORE_REPORT - 1 {
            tail.note_warming("Warming");
            assert!(!tail.warming_reported, "a normally-warming server must stay quiet");
        }
        tail.note_warming("Warming");
        assert!(tail.warming_reported, "a server that never warms must be reported");
        tail.note_warming("Warming");
        assert!(tail.warming_reported, "reported ONCE, not on every later pass");

        // Any progress at all clears the streak, so a later cold start reports afresh.
        tail.note_warming("Completed");
        assert_eq!(tail.warming_passes, 0);
        assert!(!tail.warming_reported);
    }

    /// A `MakeWriter` that appends every formatted log line into a shared buffer, so a test can
    /// assert on the `tracing` events a pass actually emitted — and on how many times.
    #[derive(Clone)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `body` with warnings captured, returning what it logged. The subscriber is thread-local
    /// (`with_default`), so parallel tests do not see each other's output.
    fn captured_warnings(body: impl FnOnce()) -> String {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(CaptureWriter(std::sync::Arc::clone(&buffer)))
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let logged = buffer.lock().unwrap().clone();
        String::from_utf8(logged).expect("formatted log lines are UTF-8")
    }

    #[test]
    fn a_backend_that_can_configure_nothing_is_reported_once_then_stays_quiet() {
        // A candidate skipped because the session cannot configure its file is deliberately NOT
        // deferred — retrying cannot help until the checkout's layout changes. So a pass made
        // entirely of such skips writes no rows and leaves no backlog, and without a report of its
        // own the backend resolves nothing pass after pass while saying nothing at all.
        let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
        let all_skipped =
            || LivePassReport { skipped_unconfigured: 3, ..LivePassReport::default() };
        let occurrences = |logged: &str| logged.matches("cannot configure their files").count();

        let logged = captured_warnings(|| {
            tail.note_unconfigured(&all_skipped());
            tail.note_unconfigured(&all_skipped());
        });
        assert_eq!(occurrences(&logged), 1, "reported ONCE, not on every later pass: {logged:?}");

        // A pass that issues a request proves the session configures something here, so the
        // report is not repeated for it…
        let quiet = captured_warnings(|| {
            tail.note_unconfigured(&LivePassReport {
                requests_used: 1,
                skipped_unconfigured: 1,
                ..LivePassReport::default()
            });
        });
        assert_eq!(occurrences(&quiet), 0, "a pass that resolves anything is not a dry spell");
        // …and the streak is cleared, so a later all-skipped pass reports afresh.
        let again = captured_warnings(|| tail.note_unconfigured(&all_skipped()));
        assert_eq!(occurrences(&again), 1, "a new dry spell must be reported: {again:?}");
    }

    #[test]
    fn an_unconfigured_warning_names_each_database_cause() {
        struct OneSourceCorpus;

        impl IndexedCorpus for OneSourceCorpus {
            fn indexes_file(&self, absolute: &std::path::Path) -> bool {
                absolute.ends_with("src/main.c")
            }

            fn may_hold_indexed_files(&self, _dir: &std::path::Path) -> bool {
                true
            }
        }

        let warning = |report: LivePassReport| {
            let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
            captured_warnings(|| tail.note_unconfigured(&report))
        };

        // No database fact set: the terminal branch. It must OFFER its causes rather than pick
        // one — the set it covers is the complement of the branches above, so it cannot know.
        let unidentified =
            warning(LivePassReport { skipped_unconfigured: 1, ..LivePassReport::default() });
        assert!(
            unidentified.contains("do not identify which cause"),
            "the terminal branch must not assert a diagnosis: {unidentified:?}",
        );
        assert!(
            unidentified.contains("leave a single compilation database"),
            "…while still carrying the several-databases remedy: {unidentified:?}",
        );
        assert!(
            unidentified.contains("is not a string"),
            "…and the sole-database one: {unidentified:?}",
        );
        assert!(!unidentified.contains("names no file this checkout indexes"));

        let governs_nothing = warning(LivePassReport {
            skipped_unconfigured: 1,
            database_governs_nothing: true,
            ..LivePassReport::default()
        });
        assert!(
            governs_nothing.contains("names no file this checkout indexes"),
            "a non-governing database needs its own remedy: {governs_nothing:?}",
        );
        assert!(!governs_nothing.contains("leave a single compilation database"));

        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("src")).unwrap();
        std::fs::write(fixture.path().join("src/main.c"), "int main(void) { return 0; }\n")
            .unwrap();
        std::fs::write(
            fixture.path().join("compile_commands.json"),
            "[\n  # generated by hand\n  \
             {\"directory\":\"/x\",\"file\":\"/x/a.c\",\"command\":\"cc -c a.c\"},\n]\n",
        )
        .unwrap();
        let corpus = OneSourceCorpus;
        let scope = CheckoutScope::resolve(fixture.path(), &corpus);
        let clangd = backend(rag_rat_oracle::OracleTool::ClangdLsp);
        let layout = clangd.resolve_layout(&scope);
        assert!(
            clangd.checkout_can_signal_readiness(&scope, &layout),
            "a sole YAML-flavoured database is warmable even though this reader cannot parse it",
        );
        assert!(
            !clangd.session_can_resolve(&scope, "src/main.c", &layout),
            "the unreadable marker is not trusted for persisted resolutions while it is being \
             diagnosed",
        );

        let unreadable = warning(LivePassReport {
            skipped_unconfigured: 1,
            database_unreadable: layout.has_unreadable_database(),
            ..LivePassReport::default()
        });
        assert!(
            unreadable.contains("compile_commands.json"),
            "a sole unreadable database warning must identify its file: {unreadable:?}",
        );
        assert!(unreadable.contains("unsupported syntax"));
        assert!(unreadable.contains("unsupported entry key"));
        assert!(!unreadable.contains("leave a single compilation database"));
        assert!(!unreadable.contains("names no file this checkout indexes"));

        std::fs::write(
            fixture.path().join("compile_commands.json"),
            r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","extra":"x"}]"#,
        )
        .unwrap();
        let strict_json_unknown_key_layout = clangd.resolve_layout(&scope);
        assert!(
            strict_json_unknown_key_layout.has_unreadable_database(),
            "a strict-JSON entry with an unknown key still yields the Unknown layout fact",
        );
        let strict_json_unknown_key = warning(LivePassReport {
            skipped_unconfigured: 1,
            database_unreadable: strict_json_unknown_key_layout.has_unreadable_database(),
            ..LivePassReport::default()
        });
        assert!(
            strict_json_unknown_key.contains("may be unreadable"),
            "strict JSON with an unknown key parses, so unreadability may only be OFFERED as a \
             cause, never asserted: {strict_json_unknown_key:?}",
        );
        assert!(
            strict_json_unknown_key.contains("compile_commands.json"),
            "the warning must identify the database file: {strict_json_unknown_key:?}",
        );
        assert!(
            strict_json_unknown_key.contains("unsupported entry key"),
            "the warning must describe the accepted-problem class: {strict_json_unknown_key:?}",
        );

        // An existing marker path is recorded even when it cannot be opened. A directory with
        // the marker's name reaches the same Unknown layout fact as a file-open failure, so the
        // warning must not claim that its contents have either of the reader-level problems.
        std::fs::remove_file(fixture.path().join("compile_commands.json")).unwrap();
        std::fs::create_dir(fixture.path().join("compile_commands.json")).unwrap();
        let unreadable_marker_layout = clangd.resolve_layout(&scope);
        assert!(
            unreadable_marker_layout.has_unreadable_database(),
            "an existing but unopenable marker still reaches the unreadable-database branch",
        );
        let unreadable_marker = warning(LivePassReport {
            skipped_unconfigured: 1,
            database_unreadable: unreadable_marker_layout.has_unreadable_database(),
            ..LivePassReport::default()
        });
        assert!(
            unreadable_marker.contains("compile_commands.json"),
            "the warning must identify an unreadable marker path: {unreadable_marker:?}",
        );
        assert!(
            unreadable_marker.contains("could not be used"),
            "the warning must state the reader could not use the marker: {unreadable_marker:?}",
        );
        assert!(
            unreadable_marker.contains("may be unreadable"),
            "the warning must present unreadability as a possible cause: {unreadable_marker:?}",
        );

        // The shape that made the terminal branch lie: a SOLE database whose entries parse, so
        // the unreadable branch declines it, and whose `file` this reader cannot read as a path,
        // so `Governs::Unknown` makes the governs-nothing branch decline it too. It falls
        // through — and the fallthrough used to tell an operator with one database to leave a
        // single one.
        std::fs::remove_dir(fixture.path().join("compile_commands.json")).unwrap();
        std::fs::write(
            fixture.path().join("compile_commands.json"),
            r#"[{"directory":"/x","file":42,"command":"cc -c a.c"}]"#,
        )
        .unwrap();
        let non_string_file = clangd.resolve_layout(&scope);
        assert!(
            !non_string_file.has_unreadable_database(),
            "entries that parse are not an unreadable database",
        );
        assert!(
            !non_string_file.has_database_governing_nothing_indexed(),
            "a `file` this reader cannot read is unknown governance, not proven non-governance",
        );
        let sole_unreadable_entry = warning(LivePassReport {
            skipped_unconfigured: 1,
            database_unreadable: non_string_file.has_unreadable_database(),
            database_governs_nothing: non_string_file.has_database_governing_nothing_indexed(),
            ..LivePassReport::default()
        });
        assert!(
            sole_unreadable_entry.contains("do not identify which cause"),
            "a sole database that reaches the terminal branch must not be diagnosed as several: \
             {sole_unreadable_entry:?}",
        );
    }

    /// A pass that skipped `paths` because the session could not configure their files.
    fn all_skipped_report(paths: &[String]) -> LivePassReport {
        LivePassReport {
            skipped_unconfigured: paths.len() as u64,
            skipped_unconfigured_paths: paths.to_vec(),
            ..LivePassReport::default()
        }
    }

    /// A pass that ended early for `abort` without reaching any file.
    fn aborted_report(abort: LivePassAbort) -> LivePassReport {
        LivePassReport { abort: Some(abort), ..LivePassReport::default() }
    }

    #[test]
    fn a_path_the_session_cannot_configure_is_retained_without_scheduling_another_pass() {
        // The skip is deliberately not deferred, and a non-empty backlog is exactly what makes the
        // watcher schedule another pass — so parking these in the backlog would spin it forever on
        // work every pass can only skip again. They still have to be kept somewhere, or the layout
        // change that makes them resolvable has nothing to bring back.
        let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
        let worklist = vec!["b/main.c".to_string()];
        tail.retain_unconfigured(&worklist, &all_skipped_report(&worklist));

        assert!(tail.backlog.is_empty(), "a permanently-skipped path must not ride the backlog");
        assert_eq!(tail.unconfigured_paths, BTreeSet::from(["b/main.c".to_string()]));
        assert_eq!(
            tail.next_wake_in(Duration::from_secs(600), Instant::now()),
            None,
            "what is retained here must not schedule a pass on its own",
        );

        // Deduped across passes: re-editing the same unconfigurable file cannot grow the set.
        tail.retain_unconfigured(&worklist, &all_skipped_report(&worklist));
        assert_eq!(tail.unconfigured_paths.len(), 1);

        // A pass that carries the path and does NOT skip it drops it again — whatever it is now,
        // it is no longer a file waiting on a layout change.
        tail.retain_unconfigured(&worklist, &LivePassReport::default());
        assert!(tail.unconfigured_paths.is_empty());
    }

    #[test]
    fn a_parked_path_rides_along_with_real_work_but_never_causes_a_pass() {
        // The operator fix this exists for — consolidating the checkout's compilation databases —
        // changes no file in this backend's languages, so it can never build a worklist of its own.
        // Waiting for a pass to ABORT on the layout change is too narrow a trigger: an ordinary
        // pass re-answers "can I configure this?" against a freshly resolved layout just as well,
        // and costs nothing extra because an unconfigurable path is skipped before the request
        // budget is touched.
        let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
        let parked = vec!["b/main.c".to_string()];
        tail.retain_unconfigured(&parked, &all_skipped_report(&parked));

        // Alone, it still schedules nothing — parking these in the backlog would spin the watcher
        // forever on work every pass can only re-skip. Driven through `assemble_worklist`, which
        // is the ONE place a pass's worklist is built, so this covers the wiring and not just a
        // helper the pass might not call.
        let empty = assemble_worklist(
            Vec::new(),
            &mut tail.unconfigured_paths,
            Some(&BTreeSet::new()),
            &tail.backend,
        );
        assert!(empty.is_empty(), "a parked path must not manufacture a worklist");
        assert_eq!(tail.unconfigured_paths.len(), 1, "…and must not be consumed by trying");
        assert_eq!(
            tail.next_wake_in(Duration::from_secs(600), Instant::now()),
            None,
            "what is parked here must not schedule a pass on its own",
        );

        // But any pass that is happening anyway carries it.
        let changed = BTreeSet::from(["a/other.c".to_string()]);
        let worklist = assemble_worklist(
            Vec::new(),
            &mut tail.unconfigured_paths,
            Some(&changed),
            &tail.backend,
        );
        assert_eq!(worklist, vec!["a/other.c".to_string(), "b/main.c".to_string()]);
        assert!(tail.unconfigured_paths.is_empty(), "drained into the worklist, not copied");

        // Still unconfigurable → parked again, and the cycle settles rather than growing.
        tail.retain_unconfigured(&worklist, &all_skipped_report(&parked));
        assert_eq!(tail.unconfigured_paths, BTreeSet::from(["b/main.c".to_string()]));

        // Resolvable now → the pass carries it without skipping, and it is simply gone.
        let worklist = assemble_worklist(
            Vec::new(),
            &mut tail.unconfigured_paths,
            Some(&changed),
            &tail.backend,
        );
        tail.retain_unconfigured(&worklist, &LivePassReport::default());
        assert!(tail.unconfigured_paths.is_empty());
        assert!(tail.backlog.is_empty(), "resolving a parked path leaves nothing behind");
    }

    #[test]
    fn a_parked_path_is_not_dropped_by_an_abort_that_never_reached_it() {
        // An abort requeues the whole worklist through `unfinished_paths`, so a parked path the
        // pass was carrying rides the backlog rather than the parked set. It must not be lost in
        // the handover: `retain_unconfigured` clears carried paths, and the backlog is what brings
        // this one back.
        let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
        let parked = vec!["b/main.c".to_string()];
        tail.retain_unconfigured(&parked, &all_skipped_report(&parked));

        let changed = BTreeSet::from(["a/other.c".to_string()]);
        let worklist = assemble_worklist(
            Vec::new(),
            &mut tail.unconfigured_paths,
            Some(&changed),
            &tail.backend,
        );
        let mut aborted = aborted_report(LivePassAbort::Server);
        aborted.unfinished_paths = worklist.clone();
        tail.backlog = aborted.unfinished_paths.clone();
        tail.retain_unconfigured(&worklist, &aborted);

        assert!(
            tail.backlog.contains(&"b/main.c".to_string()),
            "a parked path the pass never reached must survive the abort: {:?}",
            tail.backlog,
        );
        assert!(tail.unconfigured_paths.is_empty(), "it is in the backlog, not parked twice");
    }

    /// A tail carrying two checkouts, both with a backlog so both count as having work.
    fn two_checkout_tail(now: Instant) -> LiveOracleTail {
        let mut tail = LiveOracleTail {
            checkouts: vec![
                CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
                CheckoutTail::new(
                    "/wt/feat".to_string(),
                    std::path::PathBuf::from("/wt/feat"),
                    now,
                ),
            ],
        };
        for checkout in &mut tail.checkouts {
            checkout.backends[0].backlog.push("src/a.rs".to_string());
        }
        tail
    }

    /// A minimal enabled-live config. These tests exercise the checkout bookkeeping, which reads
    /// only `oracle.live` and `root`.
    fn live_config(max_checkouts: usize) -> Config {
        let mut config = Config {
            trackers: Vec::new(),
            papertrail: Default::default(),
            sync: Default::default(),
            repo_id_override: None,
            database_key_pinned: true,
            database: std::path::PathBuf::from("/repo/.rag-rat/index.sqlite"),
            root: std::path::PathBuf::from("/repo"),
            targets: vec![rag_rat_base::config::ResolvedTarget {
                name: "rust".to_string(),
                language: rag_rat_base::language::Language::Rust,
                directories: vec![std::path::PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: rag_rat_base::config::TargetKind::Source,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
        };
        config.oracle.live.enabled = true;
        config.oracle.live.max_checkouts = max_checkouts;
        config
    }

    #[test]
    fn the_checkout_cap_serves_the_most_recently_worked_checkout_and_keeps_the_others_backlog() {
        // A resident language server is the expensive thing here — routinely gigabytes — and one
        // per (backend x checkout) multiplies that by the worktree fleet. The cap bounds SERVERS,
        // so a checkout outside it stops holding sessions but must not lose its work (#1010).
        let now = Instant::now();
        let mut tail = two_checkout_tail(now);
        // The linked checkout worked more recently than the base one.
        tail.checkouts[0].last_worked_at = now - Duration::from_secs(60);
        tail.checkouts[1].last_worked_at = now;

        tail.checkouts[1].had_new_work = true;

        let ranked = tail.rank();

        assert_eq!(ranked.first(), Some(&1), "the checkout being edited now leads: {ranked:?}",);
        assert_eq!(ranked.last(), Some(&0), "the older one follows it: {ranked:?}");
        assert_eq!(
            tail.checkouts[0].backends[0].backlog,
            vec!["src/a.rs".to_string()],
            "an unserved checkout defers its work — it must never be dropped",
        );
    }

    #[test]
    fn a_checkout_the_cap_excludes_still_retains_this_passs_new_paths() {
        // The cap decides which checkouts are SERVED, never which are remembered. Retaining work
        // only inside the per-backend pass — which an unserved checkout never reaches — silently
        // discarded edits made in whichever checkout happened to fall outside the cap, with
        // nothing left to retry them (#1010).
        let now = Instant::now();
        let mut tail = LiveOracleTail {
            checkouts: vec![
                CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
                CheckoutTail::new(
                    "/wt/feat".to_string(),
                    std::path::PathBuf::from("/wt/feat"),
                    now,
                ),
            ],
        };
        let config = live_config(1);
        let base = BTreeSet::from(["src/base.rs".to_string()]);
        let overlays = std::collections::BTreeMap::from([(
            "/wt/feat".to_string(),
            crate::watch::CheckoutReindex {
                source_root: std::path::PathBuf::from("/wt/feat"),
                paths: vec![std::path::PathBuf::from("src/linked.rs")],
                coverage: crate::index::ChangedPathsCoverage::Complete,
            },
        )]);

        tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &overlays }, now);
        // Both are eligible for a turn — the cap counts turns in the serve loop, not here...
        assert_eq!(tail.rank().len(), 2, "both checkouts have work to run");
        // ...and whichever the cap leaves out, BOTH kept their paths.
        assert_eq!(
            backlog_union(&tail.checkouts[0]),
            vec!["src/base.rs".to_string()],
            "the base checkout retained its path",
        );
        assert_eq!(
            backlog_union(&tail.checkouts[1]),
            vec!["src/linked.rs".to_string()],
            "the excluded checkout retained its path too — deferred, not dropped",
        );
    }

    #[test]
    fn a_waiting_checkout_is_admitted_ahead_of_one_that_has_already_been_served() {
        // Neither checkout has new work, so both are merely waiting. Ranking on "still has a
        // backlog" would stamp both with the same instant every pass and fall through to a
        // tie-break that rewards holding a session — pinning the slot to the incumbent forever.
        // The one kept out longest must go first (#1010).
        let now = Instant::now();
        let mut tail = two_checkout_tail(now);
        for checkout in &mut tail.checkouts {
            checkout.had_new_work = false;
        }
        // The incumbent was edited MORE recently and has already been served for it; the other has
        // never run. Service age must still win — comparing work recency first would let the
        // incumbent keep the slot for as long as it stays backlogged (warming, retrying), which is
        // exactly the starvation this ordering exists to prevent.
        tail.checkouts[0].last_worked_at = now;
        tail.checkouts[0].last_served_at = Some(now);
        tail.checkouts[1].last_worked_at = now - Duration::from_secs(600);
        tail.checkouts[1].last_served_at = None;

        let ranked = tail.rank();

        assert_eq!(
            ranked.first(),
            Some(&1),
            "the never-served checkout leads despite the incumbent's newer work: {ranked:?}",
        );
    }

    #[test]
    fn an_empty_base_change_set_does_not_claim_a_checkout_slot() {
        // A pass that touched only a linked worktree still reports `Some(empty)` for the base —
        // the hint means "this is a reliable superset", not "there is something in it". Admitting
        // that as work gave the base checkout a phantom claim: it tied every linked checkout on
        // recency, sorted ahead of them, spent the only slot doing nothing, and was pruned as
        // idle — every pass, so the linked checkout the edit belongs to never ran (#1010).
        let now = Instant::now();
        let mut tail = LiveOracleTail {
            checkouts: vec![
                CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
                CheckoutTail::new(
                    "/wt/feat".to_string(),
                    std::path::PathBuf::from("/wt/feat"),
                    now,
                ),
            ],
        };
        let config = live_config(1);
        let empty_base = BTreeSet::new();
        let overlays = std::collections::BTreeMap::from([(
            "/wt/feat".to_string(),
            crate::watch::CheckoutReindex {
                source_root: std::path::PathBuf::from("/wt/feat"),
                paths: vec![std::path::PathBuf::from("src/linked.rs")],
                coverage: crate::index::ChangedPathsCoverage::Complete,
            },
        )]);

        tail.absorb(
            &config,
            &LiveChangedSets { base: Some(&empty_base), overlays: &overlays },
            now,
        );
        let ranked = tail.rank();

        assert!(!tail.checkouts[0].had_new_work, "an empty base set is not work");
        assert!(
            !ranked.contains(&0),
            "the idle base checkout is not eligible for a turn: {ranked:?}",
        );
        assert!(ranked.contains(&1), "the linked checkout that actually changed is: {ranked:?}",);
    }

    #[test]
    fn a_checkout_whose_config_no_longer_indexes_the_language_is_not_admitted() {
        // A branch that DROPS its last target for a live language still reports the pruned source
        // paths as changed, so their extensions still look live. Admitting on the extension alone
        // hands the checkout a slot it cannot use: the backend takes the backlog, returns at its
        // language gate, and the checkout is pruned as idle — while a sibling with real work stays
        // unserved. Admission asks the same question the backend will (#1010).
        let now = Instant::now();
        let mut tail = LiveOracleTail {
            checkouts: vec![CheckoutTail::new(
                String::new(),
                std::path::PathBuf::from("/repo"),
                now,
            )],
        };
        // A `.rs` path — claimed by the Rust backend on extension — but nothing indexed here.
        let mut config = live_config(1);
        config.targets.clear();
        let base = BTreeSet::from(["src/a.rs".to_string()]);
        let no_overlays = std::collections::BTreeMap::new();

        tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &no_overlays }, now);

        assert!(
            !tail.checkouts[0].had_new_work,
            "a path whose language this checkout no longer indexes is not work for it",
        );
        assert!(
            backlog_union(&tail.checkouts[0]).is_empty(),
            "and it is not retained: {:?}",
            backlog_union(&tail.checkouts[0]),
        );
    }

    #[test]
    fn a_path_outside_this_checkouts_targets_is_not_admitted_even_in_a_live_language() {
        // The narrow form of the same bug: a branch drops ONE Rust target and keeps another. The
        // pruned paths still end in `.rs` and a Rust target still exists, so any predicate that
        // tests those two things independently admits the checkout — and it then holds the cap
        // slot, possibly with a resident server, for paths that are not in its corpus at all.
        // Matching each path against its own target is what closes this (#1010).
        let now = Instant::now();
        let mut tail = LiveOracleTail {
            checkouts: vec![CheckoutTail::new(
                String::new(),
                std::path::PathBuf::from("/repo"),
                now,
            )],
        };
        // The fixture indexes `src` only; this path is Rust, but under a directory that is not a
        // target of THIS checkout.
        let config = live_config(1);
        assert!(
            config
                .targets
                .iter()
                .any(|target| target.language == rag_rat_base::language::Language::Rust),
            "a Rust target must still exist, or this tests the wrong thing",
        );
        let base = BTreeSet::from(["extra/dropped.rs".to_string()]);
        let no_overlays = std::collections::BTreeMap::new();

        tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &no_overlays }, now);

        assert!(
            !tail.checkouts[0].had_new_work,
            "a Rust path outside this checkout's targets is not work for it",
        );
        assert!(
            backlog_union(&tail.checkouts[0]).is_empty(),
            "and it is not retained: {:?}",
            backlog_union(&tail.checkouts[0]),
        );

        // The control: the same language, under the target this checkout DOES index.
        let indexed = BTreeSet::from(["src/live.rs".to_string()]);
        tail.absorb(
            &config,
            &LiveChangedSets { base: Some(&indexed), overlays: &no_overlays },
            now,
        );
        assert!(
            tail.checkouts[0].had_new_work,
            "a path inside the target is still admitted — the gate must not reject everything",
        );
    }

    #[test]
    fn an_unreachable_checkout_keeps_its_backlog_and_yields_its_turn() {
        // A registered worktree that cannot be validated right now (unmounted, briefly unreadable)
        // is a DEFERRAL, not a removal: forgetting its backlog would lose work nothing can
        // rebuild, since a checkout returning with unchanged contents produces no changed paths
        // for the overlay refresh to report. Its turn is still spent, so it cannot hold the slot
        // every pass (#1010).
        let now = Instant::now();
        let mut tail = two_checkout_tail(now);

        tail.checkouts[1].defer_turn(now, "test");

        assert_eq!(
            tail.checkouts[1].backends[0].backlog,
            vec!["src/a.rs".to_string()],
            "the deferred checkout keeps its work",
        );
        assert_eq!(
            tail.checkouts[1].last_served_at,
            Some(now),
            "but it has spent its turn, so a sibling ranks ahead of it next pass",
        );
        assert!(
            !tail.checkouts[1].backends[0].lifecycle.can_respawn(now),
            "and the retry is backed off rather than re-attempted every pass",
        );
        assert!(
            tail.checkouts[1].deferred,
            "it is marked deferred, which is what keeps it in the wake computation",
        );
    }

    #[test]
    fn a_deferred_checkout_still_schedules_the_pass_that_will_retry_it() {
        // A deferred checkout had its turn and could not take it, so its backoff is a real retry
        // deadline. Excluding it from the wake computation — as an unranked checkout correctly is
        // — meant that when every retained checkout was unreachable, nothing scheduled a pass at
        // all and the work stayed stranded even after the checkouts came back (#1010).
        let now = Instant::now();
        let mut tail = two_checkout_tail(now);
        let config = live_config(1);
        // Nothing was served this pass; the sole checkout with work was unreachable.
        tail.checkouts.truncate(1);
        tail.checkouts[0].defer_turn(now, "test");

        let wake = tail.next_wake_in(&config, now);

        assert!(
            wake.is_some(),
            "a deferred checkout must schedule the pass that retries it, or its backlog is \
             stranded whenever the periodic sweep is off",
        );
    }

    #[test]
    fn a_checkout_holding_only_parked_paths_does_not_claim_a_slot() {
        // Parked paths ride along with a real worklist and can only be re-skipped on their own, so
        // a checkout holding nothing else has nothing to run. Letting it rank would spend the slot
        // on a pass that asks its server nothing — and since an unadmitted checkout schedules no
        // wake, the sibling it displaced would have its work stranded with the periodic sweep off.
        let now = Instant::now();
        let mut tail = two_checkout_tail(now);
        // The base checkout keeps only parked paths; the linked one holds real work.
        backends_of(&mut tail)[0].backlog.clear();
        backends_of(&mut tail)[0].unconfigured_paths.insert("src/parked.c".to_string());
        for checkout in &mut tail.checkouts {
            checkout.had_new_work = false;
        }

        let ranked = tail.rank();

        assert!(
            !ranked.contains(&0),
            "a parked-only checkout has nothing to run, so it is not ranked: {ranked:?}",
        );
        assert!(ranked.contains(&1), "the sibling with a real backlog is: {ranked:?}");
        assert_eq!(
            tail.checkouts[0].backends[0].unconfigured_paths.len(),
            1,
            "and its parked paths are kept, not discarded",
        );
    }

    #[test]
    fn a_change_set_no_live_backend_claims_does_not_claim_a_checkout_slot() {
        // The general form: "nonempty" is not the test. Most repos change files no live backend
        // can answer — Python, markdown, config. Admitting those gave the checkout a phantom claim
        // on the cap: it ranked as freshly worked, won the slot, resolved nothing, and was pruned
        // as idle, repeating every pass while a sibling's real backlog stayed out (#1010).
        let now = Instant::now();
        let mut tail = LiveOracleTail {
            checkouts: vec![
                CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
                CheckoutTail::new(
                    "/wt/feat".to_string(),
                    std::path::PathBuf::from("/wt/feat"),
                    now,
                ),
            ],
        };
        let config = live_config(1);
        // A busy base checkout, but every path is in a language no live backend serves.
        let base = BTreeSet::from([
            "scripts/build.py".to_string(),
            "README.md".to_string(),
            "rag-rat.toml".to_string(),
        ]);
        let overlays = std::collections::BTreeMap::from([(
            "/wt/feat".to_string(),
            crate::watch::CheckoutReindex {
                source_root: std::path::PathBuf::from("/wt/feat"),
                paths: vec![std::path::PathBuf::from("src/linked.rs")],
                coverage: crate::index::ChangedPathsCoverage::Complete,
            },
        )]);

        tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &overlays }, now);
        let ranked = tail.rank();

        assert!(
            !tail.checkouts[0].had_new_work,
            "a change set no live backend claims is not work for this stage",
        );
        assert!(!ranked.contains(&0), "so the base checkout is not ranked: {ranked:?}");
        assert!(
            backlog_union(&tail.checkouts[0]).is_empty(),
            "and nothing of it is retained: {:?}",
            backlog_union(&tail.checkouts[0]),
        );
        assert!(
            ranked.contains(&1),
            "while the linked checkout that actually changed is ranked: {ranked:?}",
        );
    }

    #[test]
    fn both_checkouts_with_work_are_eligible_for_a_turn() {
        // The cap is the whole knob: the same state that serves one checkout at the default must
        // serve both when an operator pays for it.
        let now = Instant::now();
        let mut tail = two_checkout_tail(now);
        tail.checkouts[0].last_worked_at = now - Duration::from_secs(60);

        assert_eq!(
            tail.rank().len(),
            2,
            "both checkouts have work, so both are eligible; how many actually run is the cap's \
             business in the serve loop",
        );
    }

    /// Every path a checkout is holding, across its backends.
    fn backlog_union(checkout: &CheckoutTail) -> Vec<String> {
        let mut paths: Vec<String> =
            checkout.backends.iter().flat_map(|backend| backend.backlog.iter().cloned()).collect();
        paths.sort();
        paths.dedup();
        paths
    }

    #[test]
    fn a_partial_overlay_report_still_contributes_the_paths_it_does_list() {
        // `Partial` means the list may be MISSING paths — the checkout's working-tree status read
        // failed, so dirty/untracked/deleted files never became candidates. It does NOT mean the
        // listed paths are doubtful: the committed tree-diff half still ran and every path here
        // had its overlay row written. Sound, merely incomplete.
        //
        // A best-effort freshness patch loses nothing by refreshing a subset, and the omitted half
        // surfaces on the next refresh (a partial pass clears the overlay basis). Discarding the
        // entry threw away work that was known stale and known which — and since a non-empty
        // backlog is what schedules the next wake, left nothing to schedule one (#1010).
        let now = Instant::now();
        let mut tail = LiveOracleTail {
            checkouts: vec![CheckoutTail::new(
                "/wt/feat".to_string(),
                std::path::PathBuf::from("/wt/feat"),
                now,
            )],
        };
        let config = live_config(1);
        let overlays = std::collections::BTreeMap::from([(
            "/wt/feat".to_string(),
            crate::watch::CheckoutReindex {
                source_root: std::path::PathBuf::from("/wt/feat"),
                paths: vec![std::path::PathBuf::from("src/a.rs")],
                coverage: crate::index::ChangedPathsCoverage::Partial,
            },
        )]);

        tail.absorb(&config, &LiveChangedSets { base: None, overlays: &overlays }, now);

        assert_eq!(
            backlog_union(&tail.checkouts[0]),
            vec!["src/a.rs".to_string()],
            "the paths a partial report DOES list are real committed changes, and are retained",
        );
        assert!(
            tail.checkouts[0].had_new_work,
            "and they count as work, so the checkout can be ranked and its backlog schedules a \
             wake",
        );
    }

    #[test]
    fn a_complete_overlay_report_becomes_that_checkouts_worklist() {
        // The counterpart: a complete list IS the checkout's changed set, and it is keyed to that
        // checkout rather than folded into the base one — a linked worktree's paths handed to the
        // main checkout's server would resolve against the wrong files.
        let now = Instant::now();
        let mut tail = LiveOracleTail {
            checkouts: vec![
                CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
                CheckoutTail::new(
                    "/wt/feat".to_string(),
                    std::path::PathBuf::from("/wt/feat"),
                    now,
                ),
            ],
        };
        let config = live_config(1);
        let base = BTreeSet::from(["src/base.rs".to_string()]);
        let overlays = std::collections::BTreeMap::from([(
            "/wt/feat".to_string(),
            crate::watch::CheckoutReindex {
                source_root: std::path::PathBuf::from("/wt/feat"),
                paths: vec![std::path::PathBuf::from("src/linked.rs")],
                coverage: crate::index::ChangedPathsCoverage::Complete,
            },
        )]);

        tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &overlays }, now);

        assert_eq!(
            backlog_union(&tail.checkouts[0]),
            vec!["src/base.rs".to_string()],
            "the base checkout holds only its own path",
        );
        assert_eq!(
            backlog_union(&tail.checkouts[1]),
            vec!["src/linked.rs".to_string()],
            "the linked checkout's path stays with it — never folded into the base checkout, \
             whose server reads different files",
        );
    }

    #[test]
    fn one_backends_failure_does_not_disturb_another() {
        // Backends are independent: a crash streak on one must not gate the other's respawn, or
        // a wedged rust-analyzer would silently stop TypeScript resolution.
        let now = Instant::now();
        let mut tail = single_checkout_tail(now);
        backends_of(&mut tail)[0].lifecycle.on_failure(now);
        assert!(!backends_of(&mut tail)[0].lifecycle.can_respawn(now));
        assert!(
            backends_of(&mut tail)[1].lifecycle.can_respawn(now),
            "sibling backoff must be separate"
        );
    }

    #[test]
    fn respawn_backoff_doubles_and_caps_without_allowing_early_retries() {
        let mut backoff = RespawnBackoff::default();
        let mut now = Instant::now();
        for expected in [30, 60, 120, 240, 300, 300] {
            backoff.record_failure(now);
            assert!(!backoff.ready(now));
            assert_eq!(backoff.remaining(now), Some(Duration::from_secs(expected)));
            now += Duration::from_secs(expected);
            assert!(backoff.ready(now));
        }
        backoff.reset();
        assert!(backoff.ready(now));
        assert_eq!(backoff.remaining(now), None);
    }

    #[test]
    fn idle_wake_counts_from_last_session_use() {
        let last_used = Instant::now();
        assert_eq!(
            idle_wake_in(last_used, last_used + Duration::from_secs(3), Duration::from_secs(10)),
            Duration::from_secs(7),
        );
        assert_eq!(
            idle_wake_in(last_used, last_used + Duration::from_secs(10), Duration::from_secs(10)),
            Duration::ZERO,
        );
        assert_eq!(
            scheduled_idle_wake_in(
                last_used,
                last_used + Duration::from_secs(10),
                Duration::from_secs(10),
            ),
            LIVE_ORACLE_RETRY_INTERVAL,
            "an unserviced overdue deadline must not spin the maintenance loop",
        );
        assert!(should_idle_shutdown(
            false,
            last_used,
            last_used + Duration::from_secs(10),
            Duration::from_secs(10),
        ));
        assert!(
            !should_idle_shutdown(
                true,
                last_used,
                last_used + Duration::from_secs(60),
                Duration::from_secs(10),
            ),
            "pending warming work must win even when the session is otherwise idle",
        );
    }

    #[test]
    fn lifecycle_prioritizes_pending_work_and_rearms_unserviced_idle_wakes() {
        let mut lifecycle = LiveOracleLifecycle::default();
        let started = Instant::now();
        let idle = Duration::from_secs(10);
        lifecycle.on_spawned(started);
        assert_eq!(lifecycle.next_wake_in(false, idle, started), Some(idle));

        let overdue = started + Duration::from_secs(60);
        assert!(!lifecycle.idle_shutdown_due(true, idle, overdue));
        assert!(lifecycle.idle_shutdown_due(false, idle, overdue));
        assert_eq!(
            lifecycle.next_wake_in(false, idle, overdue),
            Some(LIVE_ORACLE_RETRY_INTERVAL),
            "a pass that did not service the overdue wake must not spin",
        );

        lifecycle.on_session_ended();
        assert_eq!(lifecycle.next_wake_in(false, idle, overdue), None);

        lifecycle.on_failure(overdue);
        assert!(!lifecycle.can_respawn(overdue));
        assert_eq!(lifecycle.next_wake_in(true, idle, overdue), Some(LIVE_ORACLE_RETRY_INTERVAL),);
    }
}
