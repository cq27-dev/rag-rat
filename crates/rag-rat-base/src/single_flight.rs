//! Cross-process single-flight coalescing: one runner per key does the work while concurrent
//! triggers merge their payload into a shared marker and return, and the runner reruns until the
//! marker drains. Generalizes the papertrail-autosync / maintenance (#267) pattern into one
//! payload-generic primitive (#660).
//!
//! Three files per flight, all beside the index database:
//! - the **flight lock** — held by the one runner for the whole (possibly long, network-bound)
//!   pass;
//! - the **marker** — carries the coalesced [`FlightPayload`] queued for the runner;
//! - the **marker lock** — serializes every marker access, held for microseconds per section.
//!
//! LOCK ORDERING (deadlock-free): a runner holding the FLIGHT lock may block on the marker lock; a
//! contender holding the marker lock only ever TRY-acquires the flight lock. The non-blocking edge
//! is what makes the pair safe.
//!
//! THE EXIT HANDOFF is the whole correctness argument. A contender's "merge my payload, then
//! try-acquire the flight lock" section ([`SingleFlight::run`]) is serialized by the marker lock
//! either BEFORE the runner's final check (its payload is seen and earns a rerun) or AFTER the
//! flight lock is already free (its own try-acquire wins and IT becomes the runner). The runner
//! therefore releases the flight lock ONLY under the same marker-lock hold that observed an empty
//! marker — checking without the lock, or releasing outside it, reopens the window where a
//! coalesced payload is written into the void and never runs.
//!
//! CRASH SAFETY: a marker left by a killed runner is just a file; the next trigger becomes the
//! runner (its try-acquire wins the abandoned flight lock) and drains it. Nothing wedges.

use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use crate::locks::FileLock;

/// A payload coalesced across concurrent single-flight triggers, and serialized to the marker file.
///
/// `merge` must be associative and commutative (the marker is folded in whatever order contenders
/// and the runner interleave): a max over an ordered signal (papertrail), a set union (a
/// changed-path set), or a no-op (`()` — the marker's mere existence is the "rerun pending"
/// signal).
pub trait FlightPayload: Sized {
    /// Fold `other` into `self`, yielding a payload that covers both triggers.
    fn merge(self, other: Self) -> Self;
    /// Serialize to the marker file body (bytes, so a path payload need not be UTF-8).
    fn encode(&self) -> Vec<u8>;
    /// Reconstruct from a marker file body written by [`Self::encode`].
    fn decode(encoded: &[u8]) -> Self;
}

/// The maintenance case (#267): the marker's presence is the whole signal, the body is empty.
impl FlightPayload for () {
    fn merge(self, (): Self) {}
    fn encode(&self) -> Vec<u8> {
        Vec::new()
    }
    fn decode(_: &[u8]) {}
}

/// What one `run_fn` pass decided (returned to [`SingleFlight::drain`]).
pub enum Step<R> {
    /// The pass ran. Keep draining the marker; `R` is this pass's result (the last one is
    /// returned).
    Ran(R),
    /// Stop the flight, LEAVING the payload in the marker for a future trigger to pick up (e.g. the
    /// repo is not indexed yet — nothing can run, but the signal must not drop). The payload is
    /// merged back under the exit-handoff hold; the outcome is [`FlightOutcome::Stopped`]`(None)`.
    StopRequeue,
    /// Stop the flight, ABSORBING any queued marker into the payload and handing it back to the
    /// caller (e.g. the flight lock's identity key went stale — the caller re-keys and requeues
    /// elsewhere). The outcome is [`FlightOutcome::Stopped`]`(Some(payload))`.
    StopCarry,
}

/// What a whole flight did.
pub enum FlightOutcome<R, P> {
    /// A runner already held the flight lock; this caller merged its payload into the marker and
    /// returned. The holder's drain covers it.
    Coalesced,
    /// This caller ran the flight (absorbing any coalesced follow-ups). The LAST pass's result, or
    /// `None` when the drain started empty (a follow-up drain with no queued work).
    Ran(Option<R>),
    /// A `run_fn` stopped the flight early: `Some(payload)` for [`Step::StopCarry`] (handed back),
    /// `None` for [`Step::StopRequeue`] (left in the marker).
    Stopped(Option<P>),
}

/// A single-flight coordinator for one key (per-repo, per-purpose). Cheap to construct from the
/// three file paths; holds no OS resources itself.
pub struct SingleFlight<P> {
    flight_lock: PathBuf,
    marker: PathBuf,
    marker_lock: PathBuf,
    _payload: PhantomData<fn() -> P>,
}

impl<P: FlightPayload> SingleFlight<P> {
    /// Construct from the three flight paths (see the module docs). Use the `*_lock_path` /
    /// `*_pending_path` / `*_marker_lock_path` helpers in [`crate::locks`] to derive them.
    pub fn new(flight_lock: PathBuf, marker: PathBuf, marker_lock: PathBuf) -> Self {
        Self { flight_lock, marker, marker_lock, _payload: PhantomData }
    }

    /// The flight lock path — for a caller (e.g. an explicit foreground command) that acquires the
    /// flight lock itself (blocking, with a wait announcement) and then drives [`Self::drain`].
    pub fn flight_lock_path(&self) -> &Path {
        &self.flight_lock
    }

    /// Merge `payload` into the marker under the marker lock (the queue-without-running primitive):
    /// a trigger that cannot run yet (no index built, an identity re-key) parks its signal here for
    /// the next runner.
    pub fn queue(&self, payload: P) -> anyhow::Result<()> {
        self.lock_marker()?.merge(payload)
    }

    /// Consume and return the marker's queued payload under the marker lock (`None` when empty) —
    /// for a runner that must abandon its flight and CARRY the queued work elsewhere itself (e.g.
    /// an identity re-key: the stale-keyed marker is unreachable for fresh-keyed triggers).
    /// This does NOT provide the exit-handoff guarantee (a concurrent merge after the take,
    /// while the flight lock is still held, defers to the next trigger) — use it only on an
    /// abnormal abort where the caller re-parks the carried payload.
    pub fn take(&self) -> anyhow::Result<Option<P>> {
        self.lock_marker()?.take()
    }

    /// Run — or coalesce into — this key's single flight. If a runner already holds the flight
    /// lock, `payload` is merged into the marker and [`FlightOutcome::Coalesced`] returns
    /// immediately; otherwise this caller takes the flight lock and drains, starting with
    /// `payload`.
    pub fn run<R>(
        &self,
        payload: P,
        run_fn: impl FnMut(&P) -> anyhow::Result<Step<R>>,
    ) -> anyhow::Result<FlightOutcome<R, P>> {
        // Merge-then-try-acquire under ONE marker-lock hold — the contender half of the exit
        // handoff: a payload only lands in the marker while some runner is still obligated to check
        // it, and if no runner holds the flight lock THIS caller wins it and becomes the runner.
        let flight = {
            let guard = self.lock_marker()?;
            match FileLock::try_acquire(&self.flight_lock)? {
                Some(flight) => flight,
                None => {
                    guard.merge(payload)?;
                    return Ok(FlightOutcome::Coalesced);
                },
            }
        };
        self.drain(flight, Some(payload), run_fn)
    }

    /// Drive passes while holding `flight`: `initial` first (when given), then any payload
    /// coalesced into the marker mid-pass, until the exit handoff — absorbing the marker and
    /// releasing the flight lock under ONE marker-lock hold — finds nothing queued. `initial =
    /// None` drains only the follow-ups (a foreground command that already ran its own pass
    /// under the lock).
    pub fn drain<R>(
        &self,
        flight: FileLock,
        initial: Option<P>,
        mut run_fn: impl FnMut(&P) -> anyhow::Result<Step<R>>,
    ) -> anyhow::Result<FlightOutcome<R, P>> {
        let mut flight = Some(flight);
        let mut next = initial;
        let mut last = None;
        loop {
            // Fold the queued marker into `next`. On (None, None) this is the EXIT HANDOFF: release
            // the flight lock under the same hold that observed the empty marker (the `return`
            // unwinds `guard` after `flight` — flight is dropped first, marker lock still held).
            {
                let guard = self.lock_marker()?;
                match (guard.take()?, next.take()) {
                    (Some(queued), Some(current)) => next = Some(current.merge(queued)),
                    (Some(queued), None) => next = Some(queued),
                    (None, Some(current)) => next = Some(current),
                    (None, None) => {
                        drop(flight.take());
                        return Ok(FlightOutcome::Ran(last));
                    },
                }
            }
            let payload = next.take().expect("the loop is only entered with a payload");
            match run_fn(&payload) {
                Ok(Step::Ran(result)) => last = Some(result),
                Ok(Step::StopRequeue) => {
                    // Leave the payload set for a future runner, under a hold that then releases
                    // the flight lock — the one deliberate "marker without an
                    // obligated runner" exception (a not-indexed repo's inert
                    // signal).
                    let guard = self.lock_marker()?;
                    guard.merge(payload)?;
                    drop(flight.take());
                    return Ok(FlightOutcome::Stopped(None));
                },
                Ok(Step::StopCarry) => {
                    // Consume any queued marker into the payload and hand it back (the caller
                    // requeues it elsewhere — e.g. a fresh identity key).
                    let guard = self.lock_marker()?;
                    let queued = guard.take()?;
                    drop(flight.take());
                    drop(guard);
                    let carried = match queued {
                        Some(queued) => payload.merge(queued),
                        None => payload,
                    };
                    return Ok(FlightOutcome::Stopped(Some(carried)));
                },
                Err(error) => {
                    // Best-effort requeue so the failed pass's signal survives; the marker write
                    // must not shadow the error the caller is about to log. Release the flight lock
                    // UNDER THE SAME marker-lock hold (the exit handoff, exactly as the stop paths
                    // do): an errored runner bails without draining, so a contender arriving in the
                    // release window must win the flight lock and become the runner itself, never
                    // merge into a marker no live runner will drain. Dropping the lock outside the
                    // hold (at the `return`) reopens that window.
                    if let Ok(guard) = self.lock_marker() {
                        let _ = guard.merge(payload);
                        drop(flight.take());
                    }
                    return Err(error);
                },
            }
        }
    }

    fn lock_marker(&self) -> anyhow::Result<MarkerGuard<'_, P>> {
        Ok(MarkerGuard {
            marker: &self.marker,
            _update: FileLock::acquire_blocking(&self.marker_lock)?,
            _payload: PhantomData,
        })
    }
}

/// One held marker-lock section. Every marker read/write flows through here so no path can touch
/// the file outside the lock (see the module's lock-ordering + exit-handoff notes).
struct MarkerGuard<'a, P> {
    marker: &'a Path,
    _update: FileLock,
    _payload: PhantomData<fn() -> P>,
}

impl<P: FlightPayload> MarkerGuard<'_, P> {
    /// Record `payload` into the marker, merged with whatever is already queued there.
    fn merge(&self, payload: P) -> anyhow::Result<()> {
        let merged = match fs::read(self.marker) {
            Ok(existing) => payload.merge(P::decode(&existing)),
            Err(_) => payload,
        };
        fs::write(self.marker, merged.encode())?;
        Ok(())
    }

    /// Consume the marker. A payload merged concurrently is either absorbed into the returned value
    /// or lands after the removal and survives for the next check.
    fn take(&self) -> anyhow::Result<Option<P>> {
        let Ok(content) = fs::read(self.marker) else {
            return Ok(None);
        };
        let _ = fs::remove_file(self.marker);
        Ok(Some(P::decode(&content)))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;

    use super::*;

    /// A set payload — the shape the #661 changed-path trigger needs: union-merge, so a coalesced
    /// rerun must cover the UNION of everything queued (never lose a payload).
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Ids(BTreeSet<u32>);

    impl Ids {
        fn of<const N: usize>(ids: [u32; N]) -> Self {
            Self(ids.into_iter().collect())
        }
    }

    impl FlightPayload for Ids {
        fn merge(mut self, other: Self) -> Self {
            self.0.extend(other.0);
            self
        }
        fn encode(&self) -> Vec<u8> {
            self.0.iter().map(u32::to_string).collect::<Vec<_>>().join(",").into_bytes()
        }
        fn decode(encoded: &[u8]) -> Self {
            Self(
                String::from_utf8_lossy(encoded)
                    .split(',')
                    .filter_map(|token| token.parse().ok())
                    .collect(),
            )
        }
    }

    /// A fresh flight coordinator over a unique temp key (unique across threads AND processes — the
    /// coverage runner shares one process, so pid+millis alone collides).
    fn flight<P: FlightPayload>() -> (crate::test_scratch::ScratchDir, SingleFlight<P>) {
        let dir = crate::test_scratch::ScratchDir::new("single-flight");
        let sf =
            SingleFlight::new(dir.join("flight.lock"), dir.join("marker"), dir.join("marker.lock"));
        (dir, sf)
    }

    #[test]
    fn a_lone_trigger_runs_once() {
        let (_dir, sf) = flight::<Ids>();
        let passes = RefCell::new(Vec::new());
        let outcome = sf
            .run(Ids::of([1]), |p| {
                passes.borrow_mut().push(p.clone());
                Ok(Step::Ran(()))
            })
            .unwrap();
        assert!(matches!(outcome, FlightOutcome::Ran(Some(()))));
        assert_eq!(*passes.borrow(), vec![Ids::of([1])]);
    }

    #[test]
    fn a_mid_flight_trigger_earns_a_rerun_over_the_union() {
        // A trigger that coalesces WHILE the runner is mid-pass must earn one more pass covering
        // the union — never a lost payload. Simulated inline: the first pass queues a
        // second payload (as a concurrent trigger's `merge` would), and the drain must
        // rerun with it.
        let (_dir, sf) = flight::<Ids>();
        let passes = RefCell::new(Vec::new());
        let first = Cell::new(true);
        let outcome = sf
            .run(Ids::of([1]), |p| {
                passes.borrow_mut().push(p.clone());
                if first.replace(false) {
                    sf.queue(Ids::of([2, 3])).unwrap(); // a concurrent trigger coalesces mid-pass
                }
                Ok(Step::Ran(()))
            })
            .unwrap();
        assert!(matches!(outcome, FlightOutcome::Ran(Some(()))));
        assert_eq!(*passes.borrow(), vec![Ids::of([1]), Ids::of([2, 3])]);
    }

    #[test]
    fn a_trigger_coalesces_when_a_runner_holds_the_flight_lock() {
        // Holding the flight lock stands in for an in-flight runner: a fresh trigger must merge
        // into the marker and return Coalesced, not run.
        let (dir, sf) = flight::<Ids>();
        let _held = FileLock::acquire_blocking(sf.flight_lock_path()).unwrap();
        let outcome = sf
            .run(Ids::of([7]), |_| -> anyhow::Result<Step<()>> {
                panic!("must not run while the flight lock is held")
            })
            .unwrap();
        assert!(matches!(outcome, FlightOutcome::Coalesced));
        // The payload is queued for the (simulated) holder's drain.
        assert_eq!(
            std::fs::read(dir.join("marker")).map(|b| Ids::decode(&b)).unwrap(),
            Ids::of([7])
        );
    }

    #[test]
    fn a_stale_marker_is_drained_not_wedged() {
        // A marker left by a killed runner must be absorbed by the next runner's first pass, never
        // strand it. Written directly, as a crashed process would leave it.
        let (dir, sf) = flight::<Ids>();
        std::fs::write(dir.join("marker"), Ids::of([4]).encode()).unwrap();
        let passes = RefCell::new(Vec::new());
        sf.run(Ids::of([5]), |p| {
            passes.borrow_mut().push(p.clone());
            Ok(Step::Ran(()))
        })
        .unwrap();
        assert_eq!(*passes.borrow(), vec![Ids::of([4, 5])], "the stale marker is folded in, once");
        assert!(!dir.join("marker").exists(), "and the marker is drained, not left wedged");
    }

    #[test]
    fn stop_requeue_leaves_the_marker_set() {
        let (dir, sf) = flight::<Ids>();
        let outcome = sf
            .run(Ids::of([9]), |_| -> anyhow::Result<Step<()>> { Ok(Step::StopRequeue) })
            .unwrap();
        assert!(matches!(outcome, FlightOutcome::Stopped(None)));
        assert_eq!(
            std::fs::read(dir.join("marker")).map(|b| Ids::decode(&b)).unwrap(),
            Ids::of([9]),
            "the payload is left for a future runner",
        );
    }

    #[test]
    fn stop_carry_hands_the_payload_back_and_clears_the_marker() {
        let (dir, sf) = flight::<Ids>();
        // A payload also queued mid-marker must be absorbed into the carried value.
        sf.queue(Ids::of([2])).unwrap();
        let outcome =
            sf.run(Ids::of([1]), |_| -> anyhow::Result<Step<()>> { Ok(Step::StopCarry) }).unwrap();
        match outcome {
            FlightOutcome::Stopped(Some(carried)) => assert_eq!(carried, Ids::of([1, 2])),
            other =>
                panic!("expected Stopped(Some), got {:?}", matches!(other, FlightOutcome::Ran(_))),
        }
        assert!(!dir.join("marker").exists(), "the marker is consumed into the carried payload");
    }

    #[test]
    fn the_unit_payload_reruns_on_a_bare_marker() {
        // The maintenance (#267) case: `()` carries nothing; the marker's mere existence is the
        // rerun signal.
        let (_dir, sf) = flight::<()>();
        let count = Cell::new(0u32);
        let first = Cell::new(true);
        sf.run((), |()| {
            count.set(count.get() + 1);
            if first.replace(false) {
                sf.queue(()).unwrap();
            }
            Ok(Step::Ran(()))
        })
        .unwrap();
        assert_eq!(count.get(), 2, "a bare marker set mid-pass earns exactly one rerun");
    }
}
