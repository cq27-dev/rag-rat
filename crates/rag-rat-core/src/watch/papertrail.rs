//! The watcher's papertrail auto-sync trigger (#592): a periodic evaluation deadline that fires
//! even when the filesystem is idle, and a dedicated worker thread that runs the coalesced
//! cross-process flight. Deliberately SEPARATE from the maintenance pass worker — a mirror
//! flight waits on the network (pages, rate-governor sleeps) and must neither delay index passes
//! nor be delayed by them — and the deadline is computed independently of the debounce and the
//! pass-in-flight state, so an in-flight maintenance pass never postpones a probe.

use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rag_rat_base::config::Config;

use super::pass::LoopMsg;
use crate::index::papertrail::{AutosyncRequest, PapertrailContext};

/// Clock for the periodic papertrail evaluation deadline, pure like `Debounce` / `SweepClock`
/// (clock injected). `None` interval — no resolved tracker bindings, or a zeroed cadence —
/// means never due. The first tick is one full interval after startup: the scheduling policy's
/// persisted freshness makes an eager boot-time evaluation redundant, and startup already runs
/// the catch-up index pass.
#[derive(Debug)]
pub(crate) struct PapertrailClock {
    interval: Option<Duration>,
    last_tick: Instant,
}

impl PapertrailClock {
    pub(crate) fn new(interval: Option<Duration>, now: Instant) -> Self {
        Self { interval, last_tick: now }
    }

    pub(crate) fn on_tick(&mut self, now: Instant) {
        self.last_tick = now;
    }

    /// `None` when disabled OR when the configured cadence overflows `Instant` arithmetic — a
    /// deadline beyond the platform's monotonic range never arrives, and a bare `+` would panic
    /// the watcher on its first wait computation.
    fn deadline(&self) -> Option<Instant> {
        self.interval.and_then(|interval| self.last_tick.checked_add(interval))
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|at| now >= at)
    }

    pub(crate) fn due_in(&self, now: Instant) -> Option<Duration> {
        self.deadline().map(|at| at.saturating_duration_since(now))
    }
}

/// The watcher's evaluation cadence: the tightest configured deadline (the probe interval and
/// the daily full-walk backstop share one wake-up), or `None` when the repo resolves no tracker
/// bindings. Resolved ONCE at watcher startup — the config is fixed for the watcher's lifetime.
pub(crate) fn papertrail_tick_interval(config: &Config) -> Option<Duration> {
    if PapertrailContext::resolve(config).trackers.is_empty() {
        return None;
    }
    let secs = config.papertrail.probe_interval_secs.min(config.papertrail.full_sync_interval_secs);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// In-process single-flight for the papertrail worker: at most one request in the air per
/// watcher; requests arriving mid-flight coalesce into ONE pending follow-up, merged max-wins so
/// a queued full walk is never weakened by a later incremental tick. (Cross-process coalescing
/// is the flight lock + pending marker inside `autosync::run`; this keeps THIS watcher from
/// queueing redundant flights behind a slow one.)
#[derive(Debug)]
pub(crate) struct PapertrailScheduler {
    inflight: bool,
    pending: Option<AutosyncRequest>,
}

impl PapertrailScheduler {
    pub(crate) fn new() -> Self {
        Self { inflight: false, pending: None }
    }

    /// Admit a request: `Some` = send it to the worker now; `None` = coalesced into the pending
    /// follow-up.
    pub(crate) fn admit(&mut self, request: AutosyncRequest) -> Option<AutosyncRequest> {
        if self.inflight {
            self.pending = Some(self.pending.take().map_or(request, |queued| queued.max(request)));
            return None;
        }
        self.inflight = true;
        Some(request)
    }

    /// The flight finished; the caller sends the returned coalesced follow-up, if any.
    pub(crate) fn on_done(&mut self) -> Option<AutosyncRequest> {
        self.inflight = false;
        let follow_up = self.pending.take()?;
        self.admit(follow_up)
    }
}

/// Spawn the papertrail worker. Each request runs OFF the event-loop thread and answers with
/// [`LoopMsg::PapertrailDone`] so the loop dispatches the coalesced follow-up. The worker exits
/// when the request channel closes — and is deliberately NOT joined on shutdown: a flight can be
/// network-bound for minutes (rate-governor sleeps), the mirror cursor persists after every
/// page, and the flight flock dies with the process, so detaching keeps shutdown bounded while
/// an interrupted walk simply resumes on the next trigger.
pub(crate) fn spawn_papertrail_worker(
    request_rx: Receiver<AutosyncRequest>,
    done_tx: Sender<LoopMsg>,
    mut run_request: impl FnMut(AutosyncRequest) + Send + 'static,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("rag-rat-papertrail".to_string())
        .spawn(move || {
            while let Ok(request) = request_rx.recv() {
                run_request(request);
                if done_tx.send(LoopMsg::PapertrailDone).is_err() {
                    return;
                }
            }
        })
        .ok()
}
