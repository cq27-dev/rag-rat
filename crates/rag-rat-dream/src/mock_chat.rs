//! A network-free [`ChatModel`] for the dream tests. Lives here (not in `rag-rat-llm`) because it
//! is a dream-test-only helper shared across the `verdict`, `compact`, and `findings` test modules;
//! keeping it out of the library crate avoids shipping a mock in the public surface.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rag_rat_llm::chat::{ChatModel, GuidedJson};

/// A [`ChatModel`] that hands back canned completions in order and counts how many times it was
/// called (so a churn-skip test can assert the model was NOT re-invoked). When the queue drains it
/// repeats the last response, or errors if it was never given one. The guided schema is ignored —
/// the dream passes parse text markers, not JSON.
pub(crate) struct MockChatModel {
    responses: Mutex<VecDeque<String>>,
    last: Mutex<Option<String>>,
    calls: AtomicUsize,
}

impl MockChatModel {
    /// A mock that returns `responses` in order (then repeats the last one).
    pub(crate) fn new<I, S>(responses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            responses: Mutex::new(responses.into_iter().map(Into::into).collect()),
            last: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }

    /// How many times the model has been asked to complete — the churn-skip assertion hook.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ChatModel for MockChatModel {
    fn model_id(&self) -> &str {
        "mock-chat-model"
    }

    fn complete_guided(
        &self,
        _prompt: &str,
        _guided: Option<GuidedJson<'_>>,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(next) = self.responses.lock().unwrap().pop_front() {
            *self.last.lock().unwrap() = Some(next.clone());
            return Ok(next);
        }
        self.last
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("mock chat model was given no responses"))
    }
}

#[cfg(test)]
mod tests {
    use rag_rat_llm::chat::ChatModel;

    use super::MockChatModel;

    #[test]
    fn mock_returns_queued_responses_then_repeats_last_and_counts_calls() {
        let m = MockChatModel::new(["first", "second"]);
        assert_eq!(m.complete("p").unwrap(), "first");
        assert_eq!(m.complete("p").unwrap(), "second");
        assert_eq!(m.complete("p").unwrap(), "second", "drained queue repeats the last response");
        assert_eq!(m.calls(), 3);
    }

    #[test]
    fn mock_without_responses_errors() {
        let m = MockChatModel::new(Vec::<String>::new());
        assert!(m.complete("p").is_err(), "a mock given no responses errors rather than panicking");
    }
}
