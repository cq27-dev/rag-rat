//! Backend-specific readiness: how a live language server tells the client that asking for a
//! definition will produce a real answer rather than a warm-up artifact.
//!
//! Readiness is not a nicety. A server that has not finished loading its project does not
//! reliably answer `null` — `typescript-language-server` answers an imported callee with the
//! IMPORT STATEMENT in the calling file, a plausible non-null that the write path would persist as
//! a real `Upgrade`/`Contradict` verdict and (under the covered-skip budget) never revisit until
//! the file's bytes change. So each backend declares the signal it actually emits, and the pass
//! refuses to interpret a batch that straddled a non-ready window.
//!
//! Two signals cover the live backends:
//!
//! - [`ReadinessPolicy::ServerStatus`] — `rust-analyzer`'s `experimental/serverStatus`
//!   notification, an explicit session-level quiescence flag.
//! - [`ReadinessPolicy::WorkDoneProgress`] — the LSP-standard `$/progress` work-done cycle, which
//!   `typescript-language-server` emits around each project load. Readiness LATCHES: the session
//!   becomes ready when its first cycle drains, and a later load flips it back and bumps the
//!   checkpoint so an in-flight batch is discarded.
//!
//! Both encode into one [`ReadinessState`] atomic whose snapshot ([`ReadinessState::checkpoint`])
//! is indivisible, so comparing checkpoints around a batch detects even a
//! non-ready→ready cycle that the latest boolean alone would hide.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// The low bit is current readiness; the remaining bits count non-ready transitions.
const SERVER_READY_BIT: u64 = 1;

/// How a backend signals that it is ready to answer definition requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadinessPolicy {
    /// `rust-analyzer`: `experimental/serverStatus` with `quiescent: true` and a non-error health.
    /// Advertised through the `experimental.serverStatusNotification` client capability.
    ServerStatus,
    /// LSP-standard work-done progress (`$/progress` begin/end). The session is ready once it has
    /// COMPLETED at least one cycle: `typescript-language-server` starts its project load on the
    /// first `textDocument/didOpen`, not at `initialize`, so an un-warmed session has no cycle yet
    /// and must not be asked for definitions.
    WorkDoneProgress,
}

impl ReadinessPolicy {
    /// Whether a session under this policy needs a warm-up `didOpen` before it can ever report
    /// ready. `WorkDoneProgress` does: its signal is emitted in response to opening a document,
    /// so waiting for readiness without opening one waits forever.
    pub(crate) fn needs_warmup_open(self) -> bool {
        matches!(self, Self::WorkDoneProgress)
    }
}

/// The readiness bits shared between the reader pump (which writes them) and the client (which
/// snapshots them).
#[derive(Debug, Default)]
pub(crate) struct ReadinessState {
    bits: AtomicU64,
}

impl ReadinessState {
    /// A checkpoint when the server is ready, `None` otherwise. The value is the non-ready
    /// transition count, so two equal checkpoints prove no reload happened in between.
    pub(crate) fn checkpoint(&self) -> Option<u64> {
        let bits = self.bits.load(Ordering::SeqCst);
        (bits & SERVER_READY_BIT != 0).then_some(bits >> 1)
    }

    fn mark_ready(&self) {
        self.bits.fetch_or(SERVER_READY_BIT, Ordering::SeqCst);
    }

    fn mark_not_ready(&self) {
        let _ = self.bits.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |bits| {
            Some(bits.wrapping_add(2) & !SERVER_READY_BIT)
        });
    }

    #[cfg(test)]
    pub(crate) fn assume_ready(&self) {
        self.mark_ready();
    }
}

/// The reader pump's readiness bookkeeping: interprets the server's notifications under the
/// backend's policy and folds them into the shared [`ReadinessState`]. Owned by the single reader
/// thread, so the in-flight token set needs no synchronization.
pub(crate) struct ReadinessTracker {
    policy: ReadinessPolicy,
    state: Arc<ReadinessState>,
    /// Work-done progress tokens the server has begun and not yet ended. Every `$/progress` we
    /// see is server-created work-done progress: partial-result progress only flows for a
    /// `partialResultToken` the client supplied, and this client never sends one.
    ///
    /// Keyed by the token's JSON value, not its rendering: LSP allows a `ProgressToken` to be an
    /// integer OR a string, so flattening both to text would let `7` and `"7"` share a slot —
    /// ending either would empty the set and report ready while the other load is still running.
    in_flight: HashSet<ProgressToken>,
}

/// An LSP `ProgressToken`, which the protocol defines as `integer | string`. Kept as a typed value
/// so two tokens are equal only when the server meant them to be.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ProgressToken {
    Number(String),
    Text(String),
}

impl ReadinessTracker {
    pub(crate) fn new(policy: ReadinessPolicy, state: Arc<ReadinessState>) -> Self {
        Self { policy, state, in_flight: HashSet::new() }
    }

    /// Fold a server message into the readiness state. Returns whether the message WAS a readiness
    /// signal — the caller drops those instead of queueing them, so an idle server's progress
    /// chatter cannot occupy the bounded response queue.
    pub(crate) fn observe(&mut self, message: &Value) -> bool {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return false;
        };
        match (self.policy, method) {
            (ReadinessPolicy::ServerStatus, "experimental/serverStatus") => {
                if server_status_is_ready(&message["params"]) {
                    self.state.mark_ready();
                } else {
                    self.state.mark_not_ready();
                }
                true
            },
            (ReadinessPolicy::WorkDoneProgress, "$/progress") => {
                self.observe_progress(&message["params"]);
                true
            },
            _ => false,
        }
    }

    fn observe_progress(&mut self, params: &Value) {
        let Some(token) = progress_token(params) else {
            return;
        };
        match params["value"].get("kind").and_then(Value::as_str) {
            Some("begin") => {
                self.in_flight.insert(token);
                self.state.mark_not_ready();
            },
            Some("end") => {
                // Only a token we saw BEGIN can end. A stray `end` must not fabricate readiness
                // for a session that has never completed a cycle — that is exactly the warm-up
                // window whose answers are wrong.
                let ended_a_tracked_cycle = self.in_flight.remove(&token);
                if ended_a_tracked_cycle && self.in_flight.is_empty() {
                    self.state.mark_ready();
                }
            },
            // `report` carries percentage/message updates only.
            _ => {},
        }
    }
}

/// The `token` of a `$/progress` notification, preserving whether the server sent it as a number
/// or a string (see [`ProgressToken`]).
fn progress_token(params: &Value) -> Option<ProgressToken> {
    match params.get("token")? {
        Value::String(token) => Some(ProgressToken::Text(token.clone())),
        Value::Number(token) => Some(ProgressToken::Number(token.to_string())),
        _ => None,
    }
}

/// rust-analyzer is ready when it reports itself quiescent and not in an error health state.
fn server_status_is_ready(params: &Value) -> bool {
    params.get("quiescent").and_then(Value::as_bool) == Some(true)
        && params.get("health").and_then(Value::as_str) != Some("error")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tracker(policy: ReadinessPolicy) -> (ReadinessTracker, Arc<ReadinessState>) {
        let state = Arc::new(ReadinessState::default());
        (ReadinessTracker::new(policy, Arc::clone(&state)), state)
    }

    #[test]
    fn a_session_starts_not_ready_under_every_policy() {
        // Unknown status is deliberately not ready: asking before the first signal turns warm-up
        // answers into permanent evidence.
        for policy in [ReadinessPolicy::ServerStatus, ReadinessPolicy::WorkDoneProgress] {
            let (_tracker, state) = tracker(policy);
            assert_eq!(state.checkpoint(), None, "{policy:?} must start not ready");
        }
    }

    #[test]
    fn server_status_tracks_quiescent_non_error_readiness() {
        let (mut tracker, state) = tracker(ReadinessPolicy::ServerStatus);
        assert!(tracker.observe(&json!({
            "method": "experimental/serverStatus",
            "params": {"health": "ok", "quiescent": true}
        })));
        assert!(state.checkpoint().is_some());
        tracker.observe(&json!({
            "method": "experimental/serverStatus",
            "params": {"health": "ok", "quiescent": false}
        }));
        assert_eq!(state.checkpoint(), None, "a reload drops readiness");
        tracker.observe(&json!({
            "method": "experimental/serverStatus",
            "params": {"health": "error", "quiescent": true}
        }));
        assert_eq!(state.checkpoint(), None, "error health is never ready");
    }

    #[test]
    fn work_done_progress_becomes_ready_only_after_a_full_cycle() {
        let (mut tracker, state) = tracker(ReadinessPolicy::WorkDoneProgress);
        assert!(tracker.observe(&json!({
            "method": "$/progress",
            "params": {"token": "t1", "value": {"kind": "begin", "title": "loading"}}
        })));
        assert_eq!(state.checkpoint(), None, "a load in flight is not ready");
        tracker.observe(&json!({
            "method": "$/progress",
            "params": {"token": "t1", "value": {"kind": "report", "percentage": 50}}
        }));
        assert_eq!(state.checkpoint(), None, "a report is not an end");
        tracker.observe(&json!({
            "method": "$/progress", "params": {"token": "t1", "value": {"kind": "end"}}
        }));
        assert!(state.checkpoint().is_some(), "the drained cycle latches ready");
    }

    #[test]
    fn a_later_load_bumps_the_checkpoint_so_a_straddling_batch_is_discarded() {
        // A second project loading mid-pass must invalidate the batch, not merely be invisible:
        // the checkpoint the pass captured before the batch must no longer compare equal.
        let (mut tracker, state) = tracker(ReadinessPolicy::WorkDoneProgress);
        for kind in ["begin", "end"] {
            tracker.observe(&json!({
                "method": "$/progress", "params": {"token": "t1", "value": {"kind": kind}}
            }));
        }
        let before = state.checkpoint().expect("ready after the first cycle");
        for kind in ["begin", "end"] {
            tracker.observe(&json!({
                "method": "$/progress", "params": {"token": "t2", "value": {"kind": kind}}
            }));
        }
        let after = state.checkpoint().expect("ready again after the second cycle");
        assert_ne!(before, after, "a completed reload must not look like no reload at all");
    }

    #[test]
    fn concurrent_progress_cycles_stay_not_ready_until_the_last_one_drains() {
        let (mut tracker, state) = tracker(ReadinessPolicy::WorkDoneProgress);
        for token in ["a", "b"] {
            tracker.observe(&json!({
                "method": "$/progress", "params": {"token": token, "value": {"kind": "begin"}}
            }));
        }
        tracker.observe(&json!({
            "method": "$/progress", "params": {"token": "a", "value": {"kind": "end"}}
        }));
        assert_eq!(state.checkpoint(), None, "one outstanding load still blocks");
        tracker.observe(&json!({
            "method": "$/progress", "params": {"token": "b", "value": {"kind": "end"}}
        }));
        assert!(state.checkpoint().is_some());
    }

    #[test]
    fn a_stray_end_cannot_fabricate_readiness() {
        // Without a matching begin there was no observed cycle, so the session is still in the
        // warm-up window whose answers are wrong.
        let (mut tracker, state) = tracker(ReadinessPolicy::WorkDoneProgress);
        tracker.observe(&json!({
            "method": "$/progress", "params": {"token": "ghost", "value": {"kind": "end"}}
        }));
        assert_eq!(state.checkpoint(), None);
    }

    #[test]
    fn integer_progress_tokens_pair_with_their_own_end() {
        let (mut tracker, state) = tracker(ReadinessPolicy::WorkDoneProgress);
        tracker.observe(&json!({
            "method": "$/progress", "params": {"token": 7, "value": {"kind": "begin"}}
        }));
        tracker.observe(&json!({
            "method": "$/progress", "params": {"token": 7, "value": {"kind": "end"}}
        }));
        assert!(state.checkpoint().is_some());
    }

    #[test]
    fn an_integer_token_never_shares_a_slot_with_the_same_digits_as_a_string() {
        // LSP's ProgressToken is `integer | string`, so `7` and `"7"` are DIFFERENT tokens. If
        // both keyed one slot, ending either would empty the set and report ready while the other
        // load was still running — persisting a warming server's answers, the exact failure this
        // whole policy exists to prevent.
        let (mut tracker, state) = tracker(ReadinessPolicy::WorkDoneProgress);
        for token in [json!(7), json!("7")] {
            tracker.observe(&json!({
                "method": "$/progress", "params": {"token": token, "value": {"kind": "begin"}}
            }));
        }
        tracker.observe(&json!({
            "method": "$/progress", "params": {"token": 7, "value": {"kind": "end"}}
        }));
        assert_eq!(state.checkpoint(), None, "the string token's load is still in flight");
        tracker.observe(&json!({
            "method": "$/progress", "params": {"token": "7", "value": {"kind": "end"}}
        }));
        assert!(state.checkpoint().is_some(), "ready only once BOTH loads drained");
    }

    #[test]
    fn each_policy_ignores_the_other_backends_signal() {
        // A tracker must not consume (or act on) a signal from a policy it isn't running: leaving
        // it unconsumed is what keeps the message-routing honest.
        let (mut server_status, state) = tracker(ReadinessPolicy::ServerStatus);
        assert!(!server_status.observe(&json!({
            "method": "$/progress", "params": {"token": "t", "value": {"kind": "end"}}
        })));
        assert_eq!(state.checkpoint(), None);

        let (mut progress, state) = tracker(ReadinessPolicy::WorkDoneProgress);
        assert!(!progress.observe(&json!({
            "method": "experimental/serverStatus",
            "params": {"health": "ok", "quiescent": true}
        })));
        assert_eq!(state.checkpoint(), None);
    }

    #[test]
    fn only_the_progress_policy_needs_a_warmup_open() {
        assert!(!ReadinessPolicy::ServerStatus.needs_warmup_open());
        assert!(ReadinessPolicy::WorkDoneProgress.needs_warmup_open());
    }
}
