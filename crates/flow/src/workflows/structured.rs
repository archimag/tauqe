//! Operation-bound supervision of one factory-bound structured request.

use crate::api::OperationGuard;
use crate::application::context::ContextSession;
use crate::application::usage_totals::UsageReceipt;
use crate::application::{ExecutionSession, StructuredExecution};
use std::future::Future;
use std::time::Duration;
use tauqe_core::execution::budget::InputLimitExceeded;
use tauqe_core::execution::structured::{StructuredContract, StructuredError};
use tauqe_core::execution::task::TaskOutcome;
use tauqe_core::execution::EventSink;
use tauqe_core::providers::{ChatMessage, StreamEvent};

/// Technical outcome of a single request, before scenario-specific interpretation.
#[derive(Debug)]
pub(crate) enum StructuredTaskError {
    ForeignOperation,
    Cancelled,
    Stream(String),
    Request(StructuredError),
    Task(String),
    Orphaned {
        cancelled: bool,
        stream_error: Option<String>,
    },
}

/// Keep schema failures distinct so the enclosing scenario owns its retry policy.
pub(crate) enum StructuredFailure {
    Cancelled,
    InputLimit(InputLimitExceeded),
    Empty,
    Decode(String),
    Failed(String),
}

impl StructuredTaskError {
    pub(crate) fn classify(self, scenario: &str) -> StructuredFailure {
        match self {
            Self::Cancelled | Self::Request(StructuredError::Cancelled) => {
                StructuredFailure::Cancelled
            }
            Self::Request(StructuredError::Empty) => StructuredFailure::Empty,
            Self::Request(StructuredError::InputLimit(limit)) => {
                StructuredFailure::InputLimit(limit)
            }
            Self::Request(StructuredError::Decode(error)) => StructuredFailure::Decode(error),
            Self::Stream(error)
            | Self::Task(error)
            | Self::Request(StructuredError::Failed(error)) => StructuredFailure::Failed(error),
            Self::Request(StructuredError::Incomplete) => {
                StructuredFailure::Failed("Structured request closed without Done".into())
            }
            Self::ForeignOperation => StructuredFailure::Failed(format!(
                "{scenario} requires an active operation for this session"
            )),
            Self::Orphaned { cancelled, stream_error } => StructuredFailure::Failed(format!(
                "{scenario} task did not stop within its exit deadline (cancelled: {cancelled}, stream error: {})",
                stream_error.as_deref().unwrap_or("none")
            )),
        }
    }
}

/// Deliver filtered events to the caller while supervising one structured task.
/// Supervise a bound structured request in its own accounting window. The
/// accounting supervisor joins the worker and drains its sources even when
/// presentation stops at a terminal event or cancellation.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_bound<T, F, Fut, P>(
    resources: &ExecutionSession,
    context: &ContextSession,
    operation: &OperationGuard,
    execution: &StructuredExecution,
    contract: StructuredContract<T>,
    messages: Vec<ChatMessage>,
    operation_id: &str,
    deadline: Duration,
    publish: P,
    mut deliver: F,
) -> Result<(Result<T, StructuredTaskError>, UsageReceipt), StructuredTaskError>
where
    T: Send + 'static,
    F: FnMut(StreamEvent) -> Fut,
    Fut: Future<Output = ()>,
    P: Fn(&str, &crate::api::UsageTotalsSnapshot) + Send + Sync + 'static,
{
    if !resources.owns(operation) {
        return Err(StructuredTaskError::ForeignOperation);
    }
    let scope = resources.prepare_scope();
    let execution = execution.clone().with_usage(scope.binding());
    let cancel = operation.hard_cancellation();
    let (sink, mut events) = EventSink::channel();
    let (handle_tx, handle_rx) = tokio::sync::oneshot::channel();
    let supervised = scope.run(
        context,
        operation_id.to_owned(),
        publish,
        async move { execution.run(&contract, messages, &sink, &cancel).await },
        move |handle| {
            let _ = handle_tx.send(handle);
        },
        |outcome| async move { outcome },
    );
    tokio::pin!(supervised);
    let mut handle: Option<tokio::task::AbortHandle> = None;
    let mut handle_rx = Some(handle_rx);
    let hard = operation.hard_cancellation();
    let mut stream_error = None;
    let mut cancelled = false;
    let mut events_closed = false;
    let mut completion = None;
    let completion = loop {
        if !cancelled && hard.is_cancelled() {
            cancelled = true;
            if let Some(handle) = &handle {
                handle.abort();
            }
        }
        if events_closed || cancelled || stream_error.is_some() {
            if let Some(result) = completion.take() {
                break result;
            }
        }
        tokio::select! {
            biased;
            result = &mut supervised, if completion.is_none() => {
                // A successful join does not imply that all queued events have
                // reached the caller. The sender closes with the worker.
                if result.is_err() { events_closed = true; }
                completion = Some(result);
            }
            result = async {
                match handle_rx.as_mut() {
                    Some(receiver) => receiver.await,
                    None => std::future::pending().await,
                }
            }, if handle_rx.is_some() && completion.is_none() => {
                handle_rx = None;
                if let Ok(task_handle) = result {
                    operation.set_abort_handle(task_handle.clone());
                    if cancelled { task_handle.abort(); }
                    handle = Some(task_handle);
                }
            }
            _ = hard.cancelled(), if !cancelled && !events_closed => {
                cancelled = true;
                if let Some(handle) = &handle { handle.abort(); }
            }
            event = events.recv(), if !events_closed && !cancelled && stream_error.is_none() => match event {
                Some(StreamEvent::Cancelled) => {
                    cancelled = true;
                    if let Some(handle) = &handle { handle.abort(); }
                }
                Some(StreamEvent::Error(message)) => {
                    stream_error = Some(message);
                    if let Some(handle) = &handle { handle.abort(); }
                }
                Some(event) => {
                    tokio::select! {
                        biased;
                        _ = hard.cancelled() => {
                            cancelled = true;
                            if let Some(handle) = &handle { handle.abort(); }
                        }
                        _ = deliver(event) => {}
                    }
                }
                None => events_closed = true,
            },
        }
    };
    operation.clear_abort_handle();
    // A cancelled delivery must not outlive the accounted request. The scope
    // has already joined the worker and its accepted usage queue here.
    let (outcome, receipt) = completion.map_err(StructuredTaskError::Task)?;
    let result = if cancelled || hard.is_cancelled() {
        Err(StructuredTaskError::Cancelled)
    } else if let Some(message) = stream_error {
        Err(StructuredTaskError::Stream(message))
    } else {
        match outcome {
            TaskOutcome::Completed(result) => result.map_err(StructuredTaskError::Request),
            TaskOutcome::Cancelled => Err(StructuredTaskError::Cancelled),
            TaskOutcome::Failed(error) => Err(StructuredTaskError::Task(error)),
            TaskOutcome::Orphaned => Err(StructuredTaskError::Orphaned {
                cancelled: false,
                stream_error: None,
            }),
        }
    };
    let _ = deadline;
    Ok((result, receipt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use crate::application::PlanExecutionEnvironment;
    use crate::workflows::architect::ArchitectScope;
    use std::sync::{Arc, Mutex};
    use tauqe_core::config::AppConfig;
    use tauqe_core::models::{ModelRole, ModelSelection, ProviderKind};
    use tauqe_core::providers::{LlmProvider, LlmRequest, ModelUsageInfo, ProviderResult};
    use tauqe_core::review::parser::single_review_contract;
    use tokio::sync::mpsc;

    struct Script(Vec<StreamEvent>);

    #[async_trait::async_trait]
    impl LlmProvider for Script {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            _: tauqe_core::stop::HardCancellation,
        ) -> ProviderResult {
            for event in &self.0 {
                tx.send(event.clone()).await.unwrap();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn fast_completion_drains_reasoning_text_and_reset_once() {
        let root = tempfile::tempdir().unwrap();
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), history);
        let operation = app.try_begin_operation().unwrap();
        let payload =
            |summary| format!(r#"{{"title":"Audit","summary":"{summary}","findings":[]}}"#);
        app.test_set_provider_for(
            ProviderKind::OpenRouter,
            Arc::new(Script(vec![
                StreamEvent::ReasoningDelta("before".into()),
                StreamEvent::TextDelta(payload("discard")),
                StreamEvent::ResetStream,
                StreamEvent::ReasoningDelta("after".into()),
                StreamEvent::TextDelta(payload("keep")),
                StreamEvent::Usage(ModelUsageInfo {
                    total_tokens: 7,
                    ..Default::default()
                }),
                StreamEvent::Done,
            ])),
        )
        .await;
        let execution = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .for_reviewer_structured(
                &app.execution_session(),
                &operation,
                &ModelSelection::Role(ModelRole::Reviewer),
                &ArchitectScope::Review("audit".into()),
            )
            .await
            .unwrap();
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&delivered);
        let (result, receipt) = run_bound(
            &app.execution_session(),
            app.context_session(),
            &operation,
            &execution,
            single_review_contract(),
            vec![ChatMessage::user("audit")],
            "audit",
            Duration::from_secs(1),
            |_, _| {},
            move |event| {
                observed.lock().unwrap().push(event);
                async {}
            },
        )
        .await
        .unwrap();
        assert!(result.is_ok());
        {
            let events = delivered.lock().unwrap();
            let visible: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::ReasoningDelta(text) => Some(format!("reasoning:{text}")),
                    StreamEvent::TextDelta(text) => Some(format!("text:{text}")),
                    StreamEvent::ResetStream => Some("reset".into()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                visible,
                [
                    "reasoning:before",
                    "text:discard",
                    "reset",
                    "reasoning:after",
                    "text:keep"
                ]
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, StreamEvent::Usage(_)))
                    .count(),
                1
            );
        }
        assert_eq!(receipt.totals.total_tokens, 7);
        assert_eq!(receipt.snapshot.operations, 1);
        assert_eq!(app.usage_snapshot().await.session.total_tokens, 7);
    }

    #[tokio::test]
    async fn cancellation_while_delivering_does_not_resume_queued_events() {
        let root = tempfile::tempdir().unwrap();
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), history);
        let operation = app.try_begin_operation().unwrap();
        app.test_set_provider_for(
            ProviderKind::OpenRouter,
            Arc::new(Script(vec![
                StreamEvent::ReasoningDelta("first".into()),
                StreamEvent::ReasoningDelta("second".into()),
                StreamEvent::TextDelta(
                    r#"{"title":"Audit","summary":"done","findings":[]}"#.into(),
                ),
                StreamEvent::Usage(ModelUsageInfo {
                    total_tokens: 5,
                    ..Default::default()
                }),
                StreamEvent::Done,
            ])),
        )
        .await;
        let execution = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .for_reviewer_structured(
                &app.execution_session(),
                &operation,
                &ModelSelection::Role(ModelRole::Reviewer),
                &ArchitectScope::Review("audit".into()),
            )
            .await
            .unwrap();
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&delivered);
        let cancelling = app.clone();
        let (result, receipt) = run_bound(
            &app.execution_session(),
            app.context_session(),
            &operation,
            &execution,
            single_review_contract(),
            vec![ChatMessage::user("audit")],
            "audit",
            Duration::from_secs(1),
            |_, _| {},
            move |event| {
                observed.lock().unwrap().push(event);
                let app = cancelling.clone();
                async move {
                    app.cancel_active_operation();
                    std::future::pending::<()>().await;
                }
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(StructuredTaskError::Cancelled)));
        assert_eq!(delivered.lock().unwrap().len(), 1);
        assert!(receipt.snapshot.cur.is_none());
        assert_eq!(receipt.snapshot.operations, 1);
    }

    #[tokio::test]
    async fn soft_stop_does_not_interrupt_blocked_delivery() {
        let root = tempfile::tempdir().unwrap();
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), history);
        let operation = app.try_begin_operation().unwrap();
        app.test_set_provider_for(
            ProviderKind::OpenRouter,
            Arc::new(Script(vec![
                StreamEvent::ReasoningDelta("first".into()),
                StreamEvent::TextDelta(
                    r#"{"title":"Audit","summary":"done","findings":[]}"#.into(),
                ),
                StreamEvent::Usage(ModelUsageInfo {
                    total_tokens: 5,
                    ..Default::default()
                }),
                StreamEvent::Done,
            ])),
        )
        .await;
        let execution = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .for_reviewer_structured(
                &app.execution_session(),
                &operation,
                &ModelSelection::Role(ModelRole::Reviewer),
                &ArchitectScope::Review("audit".into()),
            )
            .await
            .unwrap();
        let (release, proceed) = tokio::sync::watch::channel(false);
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&delivered);
        let stopping = app.clone();
        let resources = app.execution_session();
        let result = run_bound(
            &resources,
            app.context_session(),
            &operation,
            &execution,
            single_review_contract(),
            vec![ChatMessage::user("audit")],
            "audit",
            Duration::from_secs(1),
            |_, _| {},
            move |event| {
                observed.lock().unwrap().push(event);
                let stopping = stopping.clone();
                let mut proceed = proceed.clone();
                async move {
                    if !*proceed.borrow() {
                        stopping.soft_stop_active_operation();
                        while !*proceed.borrow_and_update() {
                            proceed.changed().await.unwrap();
                        }
                    }
                }
            },
        );
        tokio::pin!(result);
        tokio::select! {
            _ = &mut result => panic!("soft stop ended delivery"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        release.send_replace(true);
        let (outcome, receipt) = result.await.unwrap();
        assert!(outcome.is_ok());
        assert_eq!(receipt.totals.total_tokens, 5);
        assert_eq!(delivered.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn hard_before_request_closes_the_accounting_window() {
        let root = tempfile::tempdir().unwrap();
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), history);
        let operation = app.try_begin_operation().unwrap();
        app.test_set_provider_for(ProviderKind::OpenRouter, Arc::new(Script(vec![])))
            .await;
        app.cancel_active_operation();
        let execution = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .for_reviewer_structured(
                &app.execution_session(),
                &operation,
                &ModelSelection::Role(ModelRole::Reviewer),
                &ArchitectScope::Review("audit".into()),
            )
            .await
            .unwrap();
        let (result, receipt) = run_bound(
            &app.execution_session(),
            app.context_session(),
            &operation,
            &execution,
            single_review_contract(),
            vec![ChatMessage::user("audit")],
            "audit",
            Duration::from_secs(1),
            |_, _| {},
            |_| async {},
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(StructuredTaskError::Cancelled)));
        assert_eq!(receipt.snapshot.operations, 1);
        assert!(receipt.snapshot.cur.is_none());
    }

    #[tokio::test]
    async fn hard_after_accepted_usage_preserves_receipt() {
        let root = tempfile::tempdir().unwrap();
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), history);
        let operation = app.try_begin_operation().unwrap();
        app.test_set_provider_for(
            ProviderKind::OpenRouter,
            Arc::new(Script(vec![
                StreamEvent::Usage(ModelUsageInfo {
                    total_tokens: 9,
                    ..Default::default()
                }),
                StreamEvent::TextDelta(
                    r#"{"title":"Audit","summary":"done","findings":[]}"#.into(),
                ),
                StreamEvent::Done,
            ])),
        )
        .await;
        let execution = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .for_reviewer_structured(
                &app.execution_session(),
                &operation,
                &ModelSelection::Role(ModelRole::Reviewer),
                &ArchitectScope::Review("audit".into()),
            )
            .await
            .unwrap();
        let cancelling = app.clone();
        let (result, receipt) = run_bound(
            &app.execution_session(),
            app.context_session(),
            &operation,
            &execution,
            single_review_contract(),
            vec![ChatMessage::user("audit")],
            "audit",
            Duration::from_secs(1),
            |_, _| {},
            move |event| {
                if matches!(event, StreamEvent::Usage(_)) {
                    cancelling.cancel_active_operation();
                }
                async {}
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(StructuredTaskError::Cancelled)));
        assert_eq!(receipt.totals.total_tokens, 9);
        assert_eq!(receipt.snapshot.operations, 1);
        assert!(receipt.snapshot.cur.is_none());
    }
}
