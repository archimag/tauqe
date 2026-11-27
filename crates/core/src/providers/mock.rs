//! Scripted, deterministic [`LlmProvider`] for offline integration testing.
//!
//! The mock replays a fixed sequence of [`MockResponse`] scripts, one per
//! provider call, and records every request it receives. This lets the whole
//! turn pipeline run inside `cargo test` without network access, reproducing
//! multi-round scenarios (context research, edit proposals, verification
//! healing, cancellation) deterministically.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::{LlmRequest, ModelUsageInfo, ProviderResult};
use crate::models::ProviderKind;
use crate::stop::HardCancellation;
use async_trait::async_trait;
use tokio::sync::mpsc;

use super::types::{ChatMessage, ReasoningEffort, ResponseFormat, StreamEvent};
use super::LlmProvider;

/// A single scripted model response replayed by [`MockLlmProvider`].
#[derive(Debug, Clone, Default)]
pub struct MockResponse {
    /// Reasoning (thinking) text emitted before the answer.
    reasoning: Option<String>,
    /// Assistant answer text, which may carry protocol markup.
    content: String,
    /// Usage accounting reported after the answer.
    usage: Option<ModelUsageInfo>,
    /// When set, the response terminates with a streaming error instead of an
    /// answer.
    error: Option<String>,
}

impl MockResponse {
    /// Builds an answer response from its text content.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    /// Builds a response that terminates with a streaming error.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }

    /// Attaches reasoning text emitted before the answer.
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    /// Attaches usage accounting reported after the answer.
    pub fn with_usage(mut self, usage: ModelUsageInfo) -> Self {
        self.usage = Some(usage);
        self
    }
}

/// A recorded provider request, captured for assertions.
#[derive(Debug, Clone)]
pub struct MockRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub response_format: Option<ResponseFormat>,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Session identifier forwarded by the caller, captured for assertions.
    pub session_id: Option<String>,
}

#[derive(Debug, Default)]
struct MockState {
    scripts: VecDeque<MockResponse>,
    requests: Vec<MockRequest>,
}

/// A deterministic [`LlmProvider`] that replays pre-scripted responses.
#[derive(Debug, Default)]
pub struct MockLlmProvider {
    state: Mutex<MockState>,
}

impl MockLlmProvider {
    /// Creates a provider with an empty script.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a provider that replays the given responses in order.
    pub fn scripted(responses: impl IntoIterator<Item = MockResponse>) -> Self {
        let provider = Self::new();
        for response in responses {
            provider.queue(response);
        }
        provider
    }

    /// Appends a scripted response.
    fn queue(&self, response: MockResponse) {
        self.with_state(|state| state.scripts.push_back(response));
    }

    /// Number of responses still waiting to be replayed.
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> usize {
        self.with_state(|state| state.scripts.len())
    }

    /// Returns every recorded request in call order.
    pub fn requests(&self) -> Vec<MockRequest> {
        self.with_state(|state| state.requests.clone())
    }

    /// Returns the most recent recorded request, if any.
    pub fn last_request(&self) -> Option<MockRequest> {
        self.with_state(|state| state.requests.last().cloned())
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut MockState) -> T) -> T {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        f(&mut state)
    }

    fn next_response(&self) -> Option<MockResponse> {
        self.with_state(|state| state.scripts.pop_front())
    }

    async fn replay(
        &self,
        request: LlmRequest<'_>,
        tx: mpsc::Sender<StreamEvent>,
        cancel: &HardCancellation,
    ) -> anyhow::Result<()> {
        self.with_state(|state| {
            state.requests.push(MockRequest {
                model: request.model.to_owned(),
                messages: request.messages,
                response_format: request.response_format,
                reasoning_effort: request.reasoning_effort,
                session_id: request.session_id.map(str::to_owned),
            });
        });

        let cancelled = cancel.is_cancelled();
        if cancelled {
            let _ = tx.send(StreamEvent::Cancelled).await;
            return Ok(());
        }

        let response = self
            .next_response()
            .ok_or_else(|| anyhow::anyhow!("MockLlmProvider script exhausted"))?;

        if let Some(reasoning) = response.reasoning {
            if !reasoning.is_empty()
                && send_or_cancel(&tx, StreamEvent::ReasoningDelta(reasoning), cancel).await
            {
                return Ok(());
            }
        }

        if let Some(error) = response.error {
            let _ = send_or_cancel(&tx, StreamEvent::Error(error), cancel).await;
            return Ok(());
        }

        if !response.content.is_empty()
            && send_or_cancel(&tx, StreamEvent::TextDelta(response.content), cancel).await
        {
            return Ok(());
        }

        if let Some(usage) = response.usage {
            // Once admitted, a receipt must not be dropped by a later Hard.
            let _ = tx.send(StreamEvent::Usage(usage)).await;
        }

        let _ = send_or_cancel(&tx, StreamEvent::Done, cancel).await;
        Ok(())
    }
}

async fn send_or_cancel(
    tx: &mpsc::Sender<StreamEvent>,
    event: StreamEvent,
    cancel: &HardCancellation,
) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = tx.try_send(StreamEvent::Cancelled);
            true
        }
        result = tx.send(event) => result.is_err(),
    }
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    fn kind(&self) -> ProviderKind {
        // A test double defaults to the OpenRouter provider kind and never
        // performs any network I/O.
        ProviderKind::OpenRouter
    }

    async fn stream(
        &self,
        request: LlmRequest<'_>,
        tx: mpsc::Sender<StreamEvent>,
        cancel: HardCancellation,
    ) -> ProviderResult {
        self.replay(request, tx, &cancel)
            .await
            .map_err(crate::providers::ProviderError::Request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop::StopSource;

    async fn run(provider: &MockLlmProvider, messages: Vec<ChatMessage>) -> Vec<StreamEvent> {
        let (tx, mut rx) = mpsc::channel(64);
        let source = StopSource::new();
        let collect = async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        };
        let stream = provider.stream(
            LlmRequest::new("test-model", messages),
            tx,
            source.token().hard_cancellation(),
        );
        let (result, events) = tokio::join!(stream, collect);
        result.expect("mock stream should succeed");
        events
    }

    #[tokio::test]
    async fn replays_reasoning_content_usage_and_done_in_order() {
        let usage = ModelUsageInfo {
            prompt_tokens: 3,
            completion_tokens: 4,
            total_tokens: 7,
            ..ModelUsageInfo::default()
        };
        let provider = MockLlmProvider::scripted([MockResponse::text("hello")
            .with_reasoning("thinking")
            .with_usage(usage)]);

        let events = run(&provider, vec![ChatMessage::user("hi")]).await;

        let summary: Vec<&str> = events
            .iter()
            .map(|event| match event {
                StreamEvent::ReasoningDelta(_) => "reasoning",
                StreamEvent::TextDelta(_) => "text",
                StreamEvent::Usage(_) => "usage",
                StreamEvent::Done => "done",
                _ => "other",
            })
            .collect();
        assert_eq!(summary, vec!["reasoning", "text", "usage", "done"]);
        assert_eq!(provider.remaining(), 0);
    }

    #[tokio::test]
    async fn error_response_emits_error_without_done() {
        let provider = MockLlmProvider::scripted([MockResponse::error("boom")]);
        let events = run(&provider, Vec::new()).await;

        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error(message) if message == "boom")));
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::Done)));
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_reports_cancelled() {
        let provider = MockLlmProvider::scripted([MockResponse::text("ignored")]);
        let (tx, mut rx) = mpsc::channel(16);
        let source = StopSource::new();
        source.request_hard();

        let result = provider
            .stream(
                LlmRequest::new("test-model", Vec::new()),
                tx,
                source.token().hard_cancellation(),
            )
            .await;
        assert!(result.is_ok());

        let event = rx.recv().await.expect("cancellation event");
        assert!(matches!(event, StreamEvent::Cancelled));
        // A cancelled turn leaves the script untouched for a later attempt.
        assert_eq!(provider.remaining(), 1);
    }

    #[tokio::test]
    async fn records_each_request_in_order() {
        let provider =
            MockLlmProvider::scripted([MockResponse::text("a"), MockResponse::text("b")]);

        let _ = run(&provider, vec![ChatMessage::user("first")]).await;
        let _ = run(&provider, vec![ChatMessage::user("second")]).await;

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].model, "test-model");
        assert_eq!(requests[0].messages[0].content, "first");
        assert_eq!(requests[1].messages[0].content, "second");
        assert!(requests[0].session_id.is_none());
    }

    #[tokio::test]
    async fn records_session_id_from_session_call() {
        let provider = MockLlmProvider::scripted([MockResponse::text("x")]);
        let (tx, mut rx) = mpsc::channel(16);
        let source = StopSource::new();

        let stream = provider.stream(
            LlmRequest {
                session_id: Some("tauqe-repl-abc"),
                ..LlmRequest::new("test-model", Vec::new())
            },
            tx,
            source.token().hard_cancellation(),
        );
        let collect = async { while rx.recv().await.is_some() {} };
        let (result, ()) = tokio::join!(stream, collect);
        result.expect("mock stream should succeed");

        let request = provider.last_request().expect("recorded request");
        assert_eq!(request.session_id.as_deref(), Some("tauqe-repl-abc"));
    }

    #[tokio::test]
    async fn records_reasoning_effort_from_effort_call() {
        let provider = MockLlmProvider::scripted([MockResponse::text("x")]);
        let (tx, mut rx) = mpsc::channel(16);
        let source = StopSource::new();

        let stream = provider.stream(
            LlmRequest {
                reasoning_effort: Some(ReasoningEffort::XHigh),
                ..LlmRequest::new("test-model", Vec::new())
            },
            tx,
            source.token().hard_cancellation(),
        );
        let collect = async { while rx.recv().await.is_some() {} };
        let (result, ()) = tokio::join!(stream, collect);
        result.expect("mock stream should succeed");

        let request = provider.last_request().expect("recorded request");
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::XHigh));
    }
}
