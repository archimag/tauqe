//! Streaming transport for one already assembled provider request.

use crate::edits::stream::EditStreamFilter;
use crate::models::ProviderKind;
use crate::providers::{LlmProvider, LlmRequest, StreamEvent};
use crate::stop::HardCancellation;
use tokio::sync::mpsc;

use super::outcome::ExecutionError;
use super::support::EventSink;
use super::usage::{UsageBinding, UsageSource};

/// Response bytes and terminal facts, before the caller applies its acceptance rules.
pub(crate) struct RequestStream {
    pub text: String,
    pub previewed_message: bool,
    pub stream_error: Option<String>,
    pub cancelled: bool,
    pub completed: bool,
}

/// A full request admitted for one transport call. Dropping this value before
/// transport also releases its registered usage source.
pub(crate) struct AdmittedRequest {
    source: UsageSource,
}

pub(crate) fn admit(
    request: &LlmRequest<'_>,
    provider: ProviderKind,
    cancel: &HardCancellation,
    limit: usize,
    usage: &UsageBinding,
) -> Result<AdmittedRequest, ExecutionError> {
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    super::budget::admit(request, provider, limit)?;
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    let source = usage.register()?;
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    Ok(AdmittedRequest { source })
}

/// Executes one frozen request and joins its event forwarder even if the provider fails.
pub(crate) async fn execute_with_limit(
    provider: &dyn LlmProvider,
    request: LlmRequest<'_>,
    filter: Box<dyn EditStreamFilter>,
    events: &EventSink,
    cancel: HardCancellation,
    limit: usize,
    usage: &UsageBinding,
) -> Result<RequestStream, ExecutionError> {
    let admitted = admit(&request, provider.kind(), &cancel, limit, usage)?;
    execute_admitted(provider, request, filter, events, cancel, admitted).await
}

/// Transport only: the caller may publish round start after admission and
/// before entering this future, without running a second budget check.
pub(crate) async fn execute_admitted(
    provider: &dyn LlmProvider,
    request: LlmRequest<'_>,
    mut filter: Box<dyn EditStreamFilter>,
    events: &EventSink,
    cancel: HardCancellation,
    admitted: AdmittedRequest,
) -> Result<RequestStream, ExecutionError> {
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    let AdmittedRequest { source } = admitted;
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
    let sink = events.clone();
    let forward = tokio::spawn(async move {
        let mut result = RequestStream {
            text: String::new(),
            previewed_message: false,
            stream_error: None,
            cancelled: false,
            completed: false,
        };
        let mut text_failed = false;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Usage(report) => {
                    source.record(&report);
                    sink.emit(StreamEvent::Usage(report));
                }
                StreamEvent::TextDelta(delta) => {
                    if !text_failed {
                        result.text.push_str(&delta);
                        for filtered in filter.push_chunk(&delta) {
                            result.previewed_message |= matches!(&filtered, StreamEvent::TextDelta(s) if !s.trim().is_empty());
                            sink.emit(filtered);
                        }
                    }
                }
                StreamEvent::ResetStream => {
                    result.text.clear();
                    result.previewed_message = false;
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
                other => sink.emit(other),
            }
        }
        for filtered in filter.finish() {
            result.previewed_message |=
                matches!(&filtered, StreamEvent::TextDelta(s) if !s.trim().is_empty());
            sink.emit(filtered);
        }
        result
    });

    let provider_result = provider.stream(request, tx, cancel.clone()).await;
    // Joining the forwarder is mandatory: Usage already accepted into its
    // channel must be recorded even when the provider stopped on Hard.
    let mut stream = forward
        .await
        .map_err(|error| ExecutionError::Other(format!("request stream task failed: {error}")))?;
    if cancel.is_cancelled() && !stream.cancelled {
        stream.cancelled = true;
        events.emit(StreamEvent::Cancelled);
    }
    provider_result.map_err(|error| ExecutionError::Provider(error.to_string()))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::super::usage::{UsageScope, UsageSink};
    use super::*;
    use crate::models::ProviderKind;
    use crate::providers::{ModelUsageInfo, ProviderError, ProviderResult};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder(Mutex<Vec<ModelUsageInfo>>);

    impl UsageSink for Recorder {
        fn record(&self, usage: &ModelUsageInfo) {
            self.0.lock().unwrap().push(usage.clone());
        }
    }

    fn binding() -> UsageBinding {
        let scope = UsageScope::new(Arc::new(Recorder::default()));
        scope.activate();
        UsageBinding::Bound(scope)
    }

    fn report(tokens: u32) -> StreamEvent {
        StreamEvent::Usage(ModelUsageInfo {
            prompt_tokens: tokens,
            total_tokens: tokens,
            ..Default::default()
        })
    }

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
            _: HardCancellation,
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
        Result<RequestStream, ExecutionError>,
        Vec<StreamEvent>,
        FilterState,
    ) {
        let state = Arc::new(Mutex::new(FilterState::default()));
        let (sink, mut rx) = EventSink::channel();
        let cancel = HardCancellation::never();
        let result = execute_with_limit(
            &script,
            LlmRequest::new("test", Vec::new()),
            Box::new(Filter(Arc::clone(&state))),
            &sink,
            cancel,
            crate::config::RequestConfig::default().max_input_tokens,
            &binding(),
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
        assert_eq!(state.resets, 1);
        assert_eq!(state.chunks, ["answer"]);
        assert!(state.finished);
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::Usage(usage) => Some((usage.prompt_tokens, usage.cached_tokens)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [(1, Some(1)), (2, Some(2)), (3, Some(3))]
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
        assert!(!result.completed);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::Usage(usage) if usage.prompt_tokens == 8 && usage.cached_tokens == Some(2)))
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
            matches!(result, Err(ExecutionError::Provider(message)) if message.contains("transport failed"))
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

    #[tokio::test]
    async fn bound_usage_survives_reset_and_terminals_without_an_event_subscriber() {
        let recorder = Arc::new(Recorder::default());
        let scope = UsageScope::new(recorder.clone());
        scope.activate();
        let (sink, rx) = EventSink::channel();
        drop(rx);
        let state = Arc::new(Mutex::new(FilterState::default()));
        let cancel = HardCancellation::never();
        let result = execute_with_limit(
            &Script {
                events: vec![
                    report(1),
                    StreamEvent::ResetStream,
                    report(2),
                    StreamEvent::Done,
                    report(3),
                    StreamEvent::Error("late".into()),
                    report(4),
                ],
                failure: true,
            },
            LlmRequest::new("test", vec![]),
            Box::new(Filter(state)),
            &sink,
            cancel,
            crate::config::RequestConfig::default().max_input_tokens,
            &UsageBinding::Bound(scope.clone()),
        )
        .await;
        assert!(matches!(result, Err(ExecutionError::Provider(_))));
        scope.stop_accepting();
        scope.drain().await;
        assert_eq!(
            recorder
                .0
                .lock()
                .unwrap()
                .iter()
                .map(|u| u.prompt_tokens)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
    }

    #[tokio::test]
    async fn dropped_waiter_drains_accepted_usage_before_scope_finishes() {
        struct Parked {
            sent: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl LlmProvider for Parked {
            fn kind(&self) -> ProviderKind {
                ProviderKind::OpenRouter
            }
            async fn stream(
                &self,
                _: LlmRequest<'_>,
                tx: mpsc::Sender<StreamEvent>,
                _: HardCancellation,
            ) -> ProviderResult {
                tx.send(report(7)).await.unwrap();
                self.sent.notify_one();
                std::future::pending().await
            }
        }
        let recorder = Arc::new(Recorder::default());
        let scope = UsageScope::new(recorder.clone());
        scope.activate();
        let sent = Arc::new(tokio::sync::Notify::new());
        let provider = Parked { sent: sent.clone() };
        let state = Arc::new(Mutex::new(FilterState::default()));
        let cancel = HardCancellation::never();
        let sink = EventSink::noop();
        let binding = UsageBinding::Bound(scope.clone());
        let mut waiter = Box::pin(execute_with_limit(
            &provider,
            LlmRequest::new("test", vec![]),
            Box::new(Filter(state)),
            &sink,
            cancel,
            crate::config::RequestConfig::default().max_input_tokens,
            &binding,
        ));
        tokio::select! {
            _ = &mut waiter => panic!("provider unexpectedly finished"),
            _ = sent.notified() => {},
        }
        drop(waiter);
        scope.stop_accepting();
        scope.drain().await;
        assert_eq!(
            recorder
                .0
                .lock()
                .unwrap()
                .iter()
                .map(|u| u.prompt_tokens)
                .collect::<Vec<_>>(),
            [7]
        );
    }

    #[tokio::test]
    async fn bound_request_is_refused_before_activation_and_after_admission_closes() {
        let recorder = Arc::new(Recorder::default());
        let scope = UsageScope::new(recorder);
        let provider = Script {
            events: vec![report(1), StreamEvent::Done],
            failure: false,
        };
        let binding = UsageBinding::Bound(scope.clone());
        for active in [false, true] {
            if active {
                scope.activate();
                scope.stop_accepting();
            }
            let cancel = HardCancellation::never();
            let result = execute_with_limit(
                &provider,
                LlmRequest::new("test", vec![]),
                Box::new(Filter(Arc::new(Mutex::new(FilterState::default())))),
                &EventSink::noop(),
                cancel,
                crate::config::RequestConfig::default().max_input_tokens,
                &binding,
            )
            .await;
            assert!(matches!(result, Err(ExecutionError::UsageNotActive)));
        }
        scope.drain().await;
    }

    #[tokio::test]
    async fn closing_races_registration_without_losing_accepted_reports() {
        let recorder = Arc::new(Recorder::default());
        let scope = UsageScope::new(recorder.clone());
        scope.activate();
        let binding = UsageBinding::Bound(scope.clone());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker = std::thread::spawn({
            let barrier = barrier.clone();
            move || {
                barrier.wait();
                if let Ok(source) = binding.register() {
                    source.record(&ModelUsageInfo {
                        prompt_tokens: 9,
                        ..Default::default()
                    });
                    true
                } else {
                    false
                }
            }
        });
        barrier.wait();
        scope.stop_accepting();
        let accepted = worker.join().unwrap();
        scope.drain().await;
        assert!(matches!(
            UsageBinding::Bound(scope).register(),
            Err(ExecutionError::UsageNotActive)
        ));
        assert_eq!(
            recorder
                .0
                .lock()
                .unwrap()
                .iter()
                .map(|u| u.prompt_tokens)
                .collect::<Vec<_>>(),
            if accepted { vec![9] } else { vec![] }
        );
    }
}
