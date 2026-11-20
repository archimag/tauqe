//! Streaming transport for one already assembled provider request.

use crate::edits::stream::EditStreamFilter;
use crate::providers::{LlmProvider, LlmRequest, StreamEvent};
use tokio::sync::{mpsc, watch};

use super::context::EventSink;
use super::outcome::SystemError;

/// Response bytes and terminal facts, before the caller applies its acceptance rules.
pub(crate) struct RequestStream {
    pub text: String,
    pub stream_error: Option<String>,
    pub cancelled: bool,
    pub completed: bool,
    pub token_usage: Option<(u32, Option<u32>)>,
}

/// Executes one frozen request and joins its event forwarder even if the provider fails.
pub(crate) async fn execute(
    provider: &dyn LlmProvider,
    request: LlmRequest<'_>,
    mut filter: Box<dyn EditStreamFilter>,
    events: &EventSink,
    cancel: watch::Receiver<bool>,
) -> Result<RequestStream, SystemError> {
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
    let sink = events.clone();
    let forward = tokio::spawn(async move {
        let mut result = RequestStream {
            text: String::new(),
            stream_error: None,
            cancelled: false,
            completed: false,
            token_usage: None,
        };
        let mut text_failed = false;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => {
                    if !text_failed {
                        result.text.push_str(&delta);
                        for filtered in filter.push_chunk(&delta) {
                            sink.emit(filtered);
                        }
                    }
                }
                StreamEvent::ResetStream => {
                    result.text.clear();
                    result.stream_error = None;
                    result.completed = false;
                    text_failed = false;
                    filter.reset();
                    sink.emit(StreamEvent::ResetStream);
                }
                StreamEvent::Error(message) => {
                    sink.emit(StreamEvent::Error(message.clone()));
                    result.stream_error = Some(message);
                    text_failed = true;
                }
                StreamEvent::Cancelled => {
                    sink.emit(StreamEvent::Cancelled);
                    result.cancelled = true;
                }
                StreamEvent::Done => {
                    result.completed = true;
                    sink.emit(StreamEvent::Done);
                }
                StreamEvent::Usage(usage) => {
                    result.token_usage = Some((usage.prompt_tokens, usage.cached_tokens));
                    sink.emit(StreamEvent::Usage(usage));
                }
                other => sink.emit(other),
            }
        }
        for filtered in filter.finish() {
            sink.emit(filtered);
        }
        result
    });

    let provider_result = provider.stream(request, tx, cancel).await;
    let stream = forward
        .await
        .map_err(|error| SystemError::Other(format!("control stream task failed: {error}")))?;
    provider_result.map_err(|error| SystemError::Provider(error.to_string()))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ProviderKind;
    use crate::providers::{ModelUsageInfo, ProviderError, ProviderResult};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FilterState {
        resets: usize,
        chunks: Vec<String>,
        finished: bool,
    }

    struct Filter(Arc<Mutex<FilterState>>);

    impl EditStreamFilter for Filter {
        fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
            self.0.lock().unwrap().chunks.push(chunk.into());
            Vec::new()
        }

        fn reset(&mut self) {
            let mut state = self.0.lock().unwrap();
            state.resets += 1;
            state.chunks.clear();
        }

        fn finish(self: Box<Self>) -> Vec<StreamEvent> {
            self.0.lock().unwrap().finished = true;
            Vec::new()
        }
    }

    struct Script {
        events: Vec<StreamEvent>,
        failure: bool,
    }

    #[async_trait::async_trait]
    impl LlmProvider for Script {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            _: watch::Receiver<bool>,
        ) -> ProviderResult {
            for event in &self.events {
                tx.send(event.clone()).await.unwrap();
            }
            if self.failure {
                Err(ProviderError::Request(anyhow::anyhow!("transport failed")))
            } else {
                Ok(())
            }
        }
    }

    async fn run(
        script: Script,
    ) -> (
        Result<RequestStream, SystemError>,
        Vec<StreamEvent>,
        FilterState,
    ) {
        let state = Arc::new(Mutex::new(FilterState::default()));
        let (sink, mut rx) = EventSink::channel();
        let (_, cancel) = watch::channel(false);
        let result = execute(
            &script,
            LlmRequest::new("test", Vec::new()),
            Box::new(Filter(Arc::clone(&state))),
            &sink,
            cancel,
        )
        .await;
        let forwarded = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let state = Arc::try_unwrap(state).unwrap_or_else(|_| panic!("filter still running"));
        (result, forwarded, state.into_inner().unwrap())
    }

    #[tokio::test]
    async fn reset_discards_partial_bytes_and_preserves_every_usage_event() {
        let usage = |tokens| {
            StreamEvent::Usage(ModelUsageInfo {
                prompt_tokens: tokens,
                cached_tokens: Some(tokens),
                ..ModelUsageInfo::default()
            })
        };
        let (result, events, state) = run(Script {
            events: vec![
                StreamEvent::TextDelta("discard".into()),
                usage(1),
                StreamEvent::ResetStream,
                StreamEvent::TextDelta("answer".into()),
                usage(2),
                usage(3),
                StreamEvent::Done,
            ],
            failure: false,
        })
        .await;
        let result = result.unwrap();
        assert_eq!(result.text, "answer");
        assert!(result.completed);
        assert_eq!(result.token_usage, Some((3, Some(3))));
        assert_eq!(state.resets, 1);
        assert_eq!(state.chunks, ["answer"]);
        assert!(state.finished);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::Usage(_)))
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn reset_does_not_discard_usage_without_a_replacement() {
        let (result, events, _) = run(Script {
            events: vec![
                StreamEvent::Usage(ModelUsageInfo {
                    prompt_tokens: 8,
                    cached_tokens: Some(2),
                    ..ModelUsageInfo::default()
                }),
                StreamEvent::Done,
                StreamEvent::ResetStream,
            ],
            failure: false,
        })
        .await;
        let result = result.unwrap();
        assert_eq!(result.token_usage, Some((8, Some(2))));
        assert!(!result.completed);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::Usage(_)))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancellation_and_stream_error_remain_distinct_terminal_facts() {
        let (result, events, state) = run(Script {
            events: vec![
                StreamEvent::TextDelta("fragment".into()),
                StreamEvent::Error("broken stream".into()),
                StreamEvent::Cancelled,
            ],
            failure: false,
        })
        .await;
        let result = result.unwrap();
        assert_eq!(result.text, "fragment");
        assert_eq!(result.stream_error.as_deref(), Some("broken stream"));
        assert!(result.cancelled);
        assert!(!result.completed);
        assert!(state.finished);
        assert!(matches!(events.last(), Some(StreamEvent::Cancelled)));
    }

    #[tokio::test]
    async fn provider_failure_joins_forwarder_after_delivering_events() {
        let (result, events, state) = run(Script {
            events: vec![StreamEvent::TextDelta("partial".into()), StreamEvent::Done],
            failure: true,
        })
        .await;
        assert!(
            matches!(result, Err(SystemError::Provider(message)) if message.contains("transport failed"))
        );
        assert!(state.finished);
        assert_eq!(state.chunks, ["partial"]);
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    #[tokio::test]
    async fn terminal_error_stops_text_but_not_reasoning_and_reset_starts_a_new_attempt() {
        let (result, events, state) = run(Script {
            events: vec![
                StreamEvent::TextDelta("first".into()),
                StreamEvent::Error("broken".into()),
                StreamEvent::ReasoningDelta("thinking".into()),
                StreamEvent::TextDelta("discard".into()),
                StreamEvent::ResetStream,
                StreamEvent::TextDelta("second".into()),
                StreamEvent::Done,
            ],
            failure: false,
        })
        .await;
        let result = result.unwrap();
        assert_eq!(result.text, "second");
        assert!(result.stream_error.is_none());
        assert_eq!(state.chunks, ["second"]);
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::ReasoningDelta(text) if text == "thinking")));
    }
}
