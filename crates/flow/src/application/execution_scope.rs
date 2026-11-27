//! Supervision of one request-bound accounting window.

use super::context::ContextSession;
use super::usage::UsageSession;
use super::usage_totals::{UsageReceipt, UsageTotalsSnapshot};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauqe_core::execution::task::{self, TaskOutcome};
use tauqe_core::execution::usage::{UsageBinding, UsageScope, UsageSink};
use tauqe_core::providers::ModelUsageInfo;

/// Preparing a binding neither opens a window nor admits provider requests.
pub(crate) struct ExecutionScope {
    usage: UsageSession,
    scope: UsageScope,
    sink: Arc<WindowSink>,
}

struct WindowSink {
    usage: UsageSession,
    window: AtomicU64,
}

impl UsageSink for WindowSink {
    fn record(&self, report: &ModelUsageInfo) {
        let window = self.window.load(Ordering::Acquire);
        if window != 0 {
            self.usage.record_window(window, report);
        }
    }
}

impl ExecutionScope {
    pub(crate) fn prepare(usage: &UsageSession) -> Self {
        let sink = Arc::new(WindowSink {
            usage: usage.clone(),
            window: AtomicU64::new(0),
        });
        Self {
            usage: usage.clone(),
            scope: UsageScope::new(sink.clone()),
            sink,
        }
    }

    pub(crate) fn binding(&self) -> UsageBinding {
        UsageBinding::Bound(self.scope.clone())
    }

    /// The supervisor owns the lease through worker exit, recovery and drain.
    /// Dropping the caller's waiter leaves the spawned supervisor running.
    /// Recovery receives the worker outcome even on panic or forced abort.
    pub(crate) async fn run<T, R, W, F, Recover, OnStart, Publish>(
        self,
        context: &ContextSession,
        operation_id: String,
        publish: Publish,
        work: W,
        on_start: OnStart,
        recover: Recover,
    ) -> Result<(R, UsageReceipt), String>
    where
        T: Send + 'static,
        R: Send + 'static,
        W: Future<Output = T> + Send + 'static,
        F: Future<Output = R> + Send + 'static,
        Recover: FnOnce(TaskOutcome<T>) -> F + Send + 'static,
        OnStart: FnOnce(tokio::task::AbortHandle) + Send + 'static,
        Publish: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        let context = context.clone();
        tokio::spawn(async move {
            let guard = self.usage.begin(&context, &operation_id, publish).await;
            self.sink.window.store(guard.window_id(), Ordering::Release);
            self.scope.activate();
            let task = tokio::spawn(work);
            on_start(task.abort_handle());
            let outcome = task::join(task).await;
            let result = recover(outcome).await;
            self.scope.stop_accepting();
            self.scope.drain().await;
            let receipt = guard.finish().await?;
            Ok((result, receipt))
        })
        .await
        .map_err(|error| format!("accounting supervisor failed: {error}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tauqe_core::execution::structured;
    use tauqe_core::execution::support::{CancellationToken, EventSink};
    use tauqe_core::execution::ExecutionError;
    use tauqe_core::plan::prompt::critique::plan_critique_contract;
    use tauqe_core::providers::{ChatMessage, LlmRequest, ProviderResult, StreamEvent};

    struct Provider;

    #[async_trait::async_trait]
    impl tauqe_core::providers::LlmProvider for Provider {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _: LlmRequest<'_>,
            tx: tokio::sync::mpsc::Sender<StreamEvent>,
            _: tauqe_core::stop::HardCancellation,
        ) -> ProviderResult {
            tx.send(StreamEvent::Usage(ModelUsageInfo {
                prompt_tokens: 7,
                total_tokens: 7,
                cost: Some(0.5),
                ..Default::default()
            }))
            .await
            .unwrap();
            tx.send(StreamEvent::TextDelta(
                r#"{"verdict":"approved","summary":"Clean","findings":[]}"#.into(),
            ))
            .await
            .unwrap();
            tx.send(StreamEvent::Done).await.unwrap();
            Ok(())
        }
    }

    fn context(root: &std::path::Path) -> ContextSession {
        ContextSession::new(root.to_path_buf())
    }

    #[tokio::test]
    async fn receipt_is_published_before_return_and_provider_usage_is_counted_once() {
        let root = tempfile::tempdir().unwrap();
        let usage = UsageSession::new(root.path());
        let scope = ExecutionScope::prepare(&usage);
        let binding = scope.binding();
        let publications = Arc::new(Mutex::new(Vec::new()));
        let recorded = publications.clone();
        let (result, receipt) = scope
            .run(
                &context(root.path()),
                "one".into(),
                move |_, snapshot| recorded.lock().unwrap().push(snapshot.clone()),
                async move {
                    structured::execute(
                        &Provider,
                        "test",
                        None,
                        None,
                        &plan_critique_contract(),
                        vec![ChatMessage::user("task")],
                        &EventSink::noop(),
                        &CancellationToken::never(),
                        tauqe_core::config::RequestConfig::default().max_input_tokens,
                        &binding,
                    )
                    .await
                },
                |_| {},
                |outcome| async move { outcome },
            )
            .await
            .unwrap();
        assert!(matches!(result, TaskOutcome::Completed(Ok(_))));
        assert_eq!(receipt.totals.total_tokens, 7);
        assert_eq!(receipt.snapshot.operations, 1);
        assert_eq!(publications.lock().unwrap().last(), Some(&receipt.snapshot));
    }

    #[tokio::test]
    async fn failure_panic_and_abort_each_close_an_empty_window() {
        let root = tempfile::tempdir().unwrap();
        let usage = UsageSession::new(root.path());
        for mode in 0..3 {
            let scope = ExecutionScope::prepare(&usage);
            let (result, receipt) = scope
                .run(
                    &context(root.path()),
                    format!("case-{mode}"),
                    |_, _| {},
                    async move {
                        match mode {
                            0 => Err::<(), _>(ExecutionError::Other("failed".into())),
                            1 => panic!("worker panicked"),
                            _ => std::future::pending().await,
                        }
                    },
                    move |handle| {
                        if mode == 2 {
                            handle.abort();
                        }
                    },
                    |outcome| async move { outcome },
                )
                .await
                .unwrap();
            assert!(matches!(
                (mode, result),
                (0, TaskOutcome::Completed(Err(_)))
                    | (1, TaskOutcome::Failed(_))
                    | (2, TaskOutcome::Cancelled)
            ));
            assert_eq!(receipt.snapshot.operations, mode as u64 + 1);
            assert!(receipt.snapshot.cur.is_none());
        }
    }

    #[tokio::test]
    async fn next_window_waits_for_recovery_and_terminal_publication() {
        let root = tempfile::tempdir().unwrap();
        let usage = UsageSession::new(root.path());
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, proceed) = tokio::sync::oneshot::channel::<()>();
        let first = ExecutionScope::prepare(&usage);
        let first_context = context(root.path());
        let running = tokio::spawn(async move {
            first
                .run(
                    &first_context,
                    "first".into(),
                    |_, _| {},
                    async { 1 },
                    |_| {},
                    move |outcome| async move {
                        entered.send(()).unwrap();
                        proceed.await.unwrap();
                        outcome
                    },
                )
                .await
        });
        ready.await.unwrap();
        let second = ExecutionScope::prepare(&usage);
        let second_context = context(root.path());
        let waiting = tokio::spawn(async move {
            second
                .run(
                    &second_context,
                    "second".into(),
                    |_, _| {},
                    async { 2 },
                    |_| {},
                    |outcome| async move { outcome },
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        release.send(()).unwrap();
        let (_, first_receipt) = running.await.unwrap().unwrap();
        let (_, second_receipt) = waiting.await.unwrap().unwrap();
        assert_eq!(first_receipt.snapshot.operations, 1);
        assert_eq!(second_receipt.snapshot.operations, 2);
    }

    #[tokio::test]
    async fn failed_persistence_does_not_hide_the_final_receipt() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".tauqe/usage.json")).unwrap();
        let usage = UsageSession::new(root.path());
        let (_, receipt) = ExecutionScope::prepare(&usage)
            .run(
                &context(root.path()),
                "failed-write".into(),
                |_, _| {},
                async {},
                |_| {},
                |outcome| async move { outcome },
            )
            .await
            .unwrap();
        assert_eq!(receipt.snapshot.operations, 1);
        assert!(receipt.snapshot.cur.is_none());
    }
}
