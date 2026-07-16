//! Off-thread probe machinery: generation tokens, the per-step registry, and the
//! quit/teardown policy. See docs/init-wizard-design.md ("Off-thread probes + cancellation").
//!
//! Slow I/O checks (model downloads, Ollama connects, ephemeral spin-up) never block the UI:
//! a worker thread runs the probe and reports its `CheckResult` back over an `mpsc`. Results
//! carry a [`ProbeId`] (`step` + `generation`); the registry **applies a result only if its
//! generation still matches the current one** for that step. Any config-changing edit to a step
//! [`bump`](ProbeRegistry::bump)s its generation, so a probe launched against the old config is
//! dropped on arrival instead of mutating the now-current state.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

use super::steps::{CheckResult, StepId};

/// Stable match token for a probe: the step it belongs to plus the step's generation at launch.
///
/// A probe reply is applied only if its `probe_id` still equals the registry's current id for
/// that step (same `generation`). This is what makes stale results inert.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ProbeId {
    pub step: StepId,
    pub generation: u64,
}

/// What a probe is doing — drives the quit/teardown policy (see [`ProbeRegistry::on_quit`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ProbeKind {
    /// A real Hugging Face model download. Detaches on quit (resumable, nothing leaks).
    Download,
    /// A connect check against an already-running endpoint (e.g. Ollama). Cheap, detaches.
    ConnectTest,
    /// A billable ephemeral box spin-up. **Must tear down on quit** — no provider backstop.
    EphemeralTest,
    /// An oracle-tool availability probe.
    OracleTool,
    /// A crates.io latest-version fetch.
    VersionCheck,
}

/// Per-step probe lifecycle state.
#[derive(Clone, Debug)]
pub(crate) enum ProbeStatus {
    /// No probe has run (or the last result was superseded by an edit).
    Idle,
    /// A probe is in flight; `kind` records what, so [`on_quit`](ProbeRegistry::on_quit) can apply
    /// the right teardown policy.
    Running(ProbeKind),
    /// The probe finished and its result was applied.
    Done { kind: ProbeKind, result: CheckResult },
}

/// A worker's reply: the result plus the [`ProbeId`] it was launched under.
#[derive(Clone, Debug)]
pub(crate) struct ProbeMsg {
    pub probe_id: ProbeId,
    pub kind: ProbeKind,
    pub result: CheckResult,
}

/// Per-step generation counters + statuses, fed by a single `mpsc` the workers send into.
///
/// `gen[step]` is the authority: it advances on every [`bump`](Self::bump) and is what
/// [`apply`](Self::apply) compares an incoming message against.
pub(crate) struct ProbeRegistry {
    generations: [u64; StepId::COUNT],
    status: [ProbeStatus; StepId::COUNT],
    rx: Receiver<ProbeMsg>,
    tx: Sender<ProbeMsg>,
    active_ephemeral: Option<EphemeralWorker>,
}

struct EphemeralWorker {
    step: StepId,
    generation: u64,
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Clone for ProbeRegistry {
    fn clone(&self) -> Self {
        let (tx, rx) = channel();
        Self {
            generations: self.generations,
            status: self.status.clone(),
            rx,
            tx,
            active_ephemeral: None,
        }
    }
}

impl ProbeRegistry {
    pub(crate) fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            generations: [0; StepId::COUNT],
            status: std::array::from_fn(|_| ProbeStatus::Idle),
            rx,
            tx,
            active_ephemeral: None,
        }
    }

    /// The current match token for `step` — what a fresh probe should be tagged with, and what
    /// [`apply`](Self::apply) checks replies against.
    pub(crate) fn current(&self, step: StepId) -> ProbeId {
        ProbeId { step, generation: self.generations[step.index()] }
    }

    /// Advance `step`'s generation. Call on every config-changing edit to that step: stale replies
    /// will no longer match, and an in-flight ephemeral worker is cancelled before its status is
    /// cleared.
    pub(crate) fn bump(&mut self, step: StepId) {
        self.cancel_active_ephemeral_for_step(step);
        self.generations[step.index()] += 1;
        self.status[step.index()] = ProbeStatus::Idle;
    }

    /// Apply a probe reply iff it is still current (`msg.probe_id == current(step)`).
    ///
    /// Returns whether it was applied. A stale message (older generation, because the step was
    /// edited after the probe launched) is dropped and leaves status untouched.
    pub(crate) fn apply(&mut self, msg: ProbeMsg) -> bool {
        let step = msg.probe_id.step;
        let generation = msg.probe_id.generation;
        let kind = msg.kind;
        if msg.probe_id == self.current(step) {
            self.status[step.index()] = ProbeStatus::Done { kind: msg.kind, result: msg.result };
            if kind == ProbeKind::EphemeralTest {
                self.join_completed_ephemeral(step, generation);
            }
            true
        } else {
            false
        }
    }

    /// The current lifecycle state of `step`'s probe.
    pub(crate) fn status(&self, step: StepId) -> &ProbeStatus {
        &self.status[step.index()]
    }

    /// Launch a probe for `step` on a worker thread.
    ///
    /// [`bump`](Self::bump)s the generation first, marks the step `Running(kind)`, then runs `work`
    /// off-thread and sends a [`ProbeMsg`] tagged with the **new** [`ProbeId`]. The reply is
    /// delivered to [`poll`](Self::poll), which only applies it if the generation still matches.
    /// For ephemeral probes, later edits also cancel and join the worker before clearing
    /// status, because the worker can own billable remote resources.
    pub(crate) fn spawn(
        &mut self,
        step: StepId,
        kind: ProbeKind,
        work: impl FnOnce() -> CheckResult + Send + 'static,
    ) {
        if kind == ProbeKind::EphemeralTest {
            self.spawn_ephemeral(step, move |_| work());
            return;
        }
        self.bump(step);
        self.status[step.index()] = ProbeStatus::Running(kind);
        let probe_id = self.current(step);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = work();
            // The receiver outlives the registry for the wizard's lifetime; if it's already
            // gone (registry dropped mid-probe) the send fails harmlessly and
            // the result is discarded.
            let _ = tx.send(ProbeMsg { probe_id, kind, result });
        });
    }

    pub(crate) fn spawn_ephemeral(
        &mut self,
        step: StepId,
        work: impl FnOnce(Arc<AtomicBool>) -> CheckResult + Send + 'static,
    ) {
        self.bump(step);
        self.status[step.index()] = ProbeStatus::Running(ProbeKind::EphemeralTest);
        let probe_id = self.current(step);
        let tx = self.tx.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let handle = thread::spawn(move || {
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            let result = work(Arc::clone(&worker_cancel));
            if !worker_cancel.load(Ordering::Acquire) {
                // The receiver outlives the registry for the wizard's lifetime; if it's already
                // gone (registry dropped mid-probe) the send fails harmlessly and the result is
                // discarded.
                let _ = tx.send(ProbeMsg { probe_id, kind: ProbeKind::EphemeralTest, result });
            }
        });
        self.active_ephemeral =
            Some(EphemeralWorker { step, generation: probe_id.generation, cancel, handle });
    }

    /// Drain every pending worker reply and [`apply`](Self::apply) each. Called once per
    /// event-loop tick. Non-blocking: returns immediately when the channel is empty.
    pub(crate) fn poll(&mut self) -> Vec<(StepId, ProbeKind, CheckResult)> {
        let mut applied = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            let step = msg.probe_id.step;
            let kind = msg.kind;
            let result = msg.result.clone();
            if self.apply(msg) {
                applied.push((step, kind, result));
            }
        }
        applied
    }

    /// Quit-time teardown policy, keyed on the in-flight [`ProbeKind`] of each `Running` step.
    ///
    /// The two probe kinds that can be airborne at quit have **opposite** correct behaviors:
    ///
    /// - [`ProbeKind::Download`] (and the other cheap/idempotent kinds — `ConnectTest`,
    ///   `OracleTool`, `VersionCheck`): **detach freely.** A partial HF download is resumable and
    ///   nothing is leaked by walking away, so we don't wait on them.
    /// - [`ProbeKind::EphemeralTest`]: **must trigger a bounded teardown.** This probe provisions a
    ///   *billable* ephemeral GPU box, and RunPod on-demand pods have **no provider-side backstop**
    ///   (unlike Modal, which self-destructs at ~30 min). Abandoning the box on quit would leak a
    ///   live, billing instance indefinitely. Quit must therefore drive the box down through the
    ///   cookbook process-group teardown machinery (SIGTERM → grace → SIGKILL) and wait only for
    ///   that bounded teardown to finish.
    ///
    /// Task 14 wired the ephemeral spin-up PATH (`steps::remote` → core `verify_ephemeral_remote` →
    /// `provision_and_build`): the worker holds the `ProvisionedBox` for the whole provision→ping
    /// round-trip, and that box's `Drop` IS the bounded SIGTERM → grace → SIGKILL process-group
    /// teardown. So a probe that runs to completion (or whose worker thread unwinds) always tears
    /// its box down. The cookbook provider ALSO installs a process-level SIGINT/SIGTERM handler
    /// (`ACTIVE_PGID`) that reaps a live process group on SIGINT/SIGTERM, so a **signal** quit
    /// (Ctrl-C / SIGTERM) is backstopped there.
    ///
    /// A clean-process-exit quit (`q`/`Esc`, no signal) is closed by Task 14b: this path calls
    /// [`abort_active_provisioning`](rag_rat_llm::providers::abort_active_provisioning), which
    /// runs the same bounded SIGTERM → grace → SIGKILL teardown against the live `ACTIVE_PGID`
    /// from outside the detached worker. It is a no-op when no box is live (`ACTIVE_PGID ==
    /// 0`), so it is safe to call unconditionally on every in-flight ephemeral probe at quit.
    pub(crate) fn on_quit(&mut self) {
        self.cancel_active_ephemeral();
        for status in &mut self.status {
            if let ProbeStatus::Running(kind) = status {
                match kind {
                    // Detach: resumable / nothing to clean up.
                    ProbeKind::Download
                    | ProbeKind::ConnectTest
                    | ProbeKind::OracleTool
                    | ProbeKind::VersionCheck => {},
                    // Billable box. Tear down the live process group from outside the detached
                    // worker: a bounded SIGTERM → grace → SIGKILL against `ACTIVE_PGID`.
                    ProbeKind::EphemeralTest => {
                        rag_rat_llm::providers::abort_active_provisioning();
                    },
                }
                *status = ProbeStatus::Idle;
            }
        }
    }

    fn cancel_active_ephemeral_for_step(&mut self, step: StepId) {
        if self.active_ephemeral.as_ref().is_some_and(|worker| worker.step == step) {
            self.cancel_active_ephemeral();
        }
    }

    fn cancel_active_ephemeral(&mut self) {
        let Some(worker) = self.active_ephemeral.take() else { return };
        worker.cancel.store(true, Ordering::Release);
        rag_rat_llm::providers::abort_active_provisioning();
        let _ = worker.handle.join();
        if self.generations[worker.step.index()] == worker.generation {
            self.status[worker.step.index()] = ProbeStatus::Idle;
        }
    }

    fn join_completed_ephemeral(&mut self, step: StepId, generation: u64) {
        let Some(worker) = self.active_ephemeral.take() else { return };
        if worker.step == step && worker.generation == generation {
            let _ = worker.handle.join();
        } else {
            self.active_ephemeral = Some(worker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_probe_result_is_dropped() {
        let mut reg = ProbeRegistry::new();
        let id0 = reg.current(StepId::Embedding);
        reg.bump(StepId::Embedding); // user edited → generation advanced
        let applied = reg.apply(ProbeMsg {
            probe_id: id0,
            kind: ProbeKind::ConnectTest,
            result: CheckResult::ok(),
        });
        assert!(!applied); // id0 is stale → dropped
        assert!(matches!(reg.status(StepId::Embedding), ProbeStatus::Idle));
        let id1 = reg.current(StepId::Embedding);
        assert!(reg.apply(ProbeMsg {
            probe_id: id1,
            kind: ProbeKind::ConnectTest,
            result: CheckResult::ok(),
        }));
    }

    #[test]
    fn spawn_then_poll_applies_result() {
        let mut reg = ProbeRegistry::new();
        reg.spawn(StepId::Integration, ProbeKind::VersionCheck, CheckResult::ok);
        // busy-wait briefly for the worker
        for _ in 0..100 {
            reg.poll();
            if matches!(reg.status(StepId::Integration), ProbeStatus::Done { .. }) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(matches!(reg.status(StepId::Integration), ProbeStatus::Done {
            kind: ProbeKind::VersionCheck,
            ..
        }));
    }

    #[test]
    fn bump_cancels_active_ephemeral_before_clearing_status() {
        use std::time::Duration;

        let mut reg = ProbeRegistry::new();
        let (started_tx, started_rx) = channel();
        reg.spawn(StepId::Embedding, ProbeKind::EphemeralTest, move || {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(20));
            CheckResult::ok()
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            reg.status(StepId::Embedding),
            ProbeStatus::Running(ProbeKind::EphemeralTest)
        ));

        reg.bump(StepId::Embedding);

        assert!(matches!(reg.status(StepId::Embedding), ProbeStatus::Idle));
        assert!(reg.poll().is_empty());
    }

    #[test]
    fn bump_signals_active_ephemeral_worker_before_joining() {
        use std::time::Duration;

        let mut reg = ProbeRegistry::new();
        let (started_tx, started_rx) = channel();
        let saw_cancel = Arc::new(AtomicBool::new(false));
        let worker_saw_cancel = Arc::clone(&saw_cancel);
        reg.spawn_ephemeral(StepId::Embedding, move |cancel| {
            let _ = started_tx.send(());
            while !cancel.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            worker_saw_cancel.store(true, Ordering::Release);
            CheckResult::ok()
        });
        started_rx.recv().unwrap();

        reg.bump(StepId::Embedding);

        assert!(saw_cancel.load(Ordering::Acquire));
        assert!(matches!(reg.status(StepId::Embedding), ProbeStatus::Idle));
        assert!(reg.poll().is_empty());
    }

    #[test]
    fn on_quit_waits_for_active_ephemeral_worker() {
        use std::time::Duration;

        let mut reg = ProbeRegistry::new();
        let (started_tx, started_rx) = channel();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        reg.spawn(StepId::Embedding, ProbeKind::EphemeralTest, move || {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(20));
            worker_finished.store(true, Ordering::Release);
            CheckResult::ok()
        });
        started_rx.recv().unwrap();

        reg.on_quit();

        assert!(finished.load(Ordering::Acquire));
        assert!(matches!(reg.status(StepId::Embedding), ProbeStatus::Idle));
    }
}
