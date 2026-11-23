//! Operation-bound supervision of one factory-bound structured request.

use crate::api::{
    abort_and_reap_with_deadline, await_with_deadline, Application, Awaited, OperationGuard,
    TaskExit,
};
use crate::application::infrastructure::execution::{ExecutionSession, StructuredExecution};
use std::future::Future;
use std::time::Duration;
use tauqe_core::execution::structured::{StructuredContract, StructuredError};
use tauqe_core::execution::{CancellationToken, EventSink};
use tauqe_core::providers::{ChatMessage, StreamEvent};

/// Technical outcome of a single request, before scenario-specific interpretation.
pub(crate) enum StructuredTaskError {
    ForeignOperation,
    Cancelled,
    Stream(String),
    Request(StructuredError),
    Task(String),
    Orphaned,
}

/// Deliver filtered events to the caller while supervising one structured task.
///
/// The caller owns usage accounting across any number of requests. In particular,
/// Done is just an event of this request, not the end of its enclosing scenario.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run<T, F, Fut>(
    application: &Application,
    operation: &OperationGuard,
    execution: &StructuredExecution,
    contract: StructuredContract<T>,
    messages: Vec<ChatMessage>,
    operation_id: &str,
    deadline: Duration,
    deliver: F,
) -> Result<T, StructuredTaskError>
where
    T: Send + 'static,
    F: FnMut(StreamEvent) -> Fut,
    Fut: Future<Output = ()>,
{
    run_with_session(
        &application.execution_session(),
        operation,
        execution,
        contract,
        messages,
        operation_id,
        deadline,
        deliver,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_with_session<T, F, Fut>(
    resources: &ExecutionSession,
    operation: &OperationGuard,
    execution: &StructuredExecution,
    contract: StructuredContract<T>,
    messages: Vec<ChatMessage>,
    operation_id: &str,
    deadline: Duration,
    mut deliver: F,
) -> Result<T, StructuredTaskError>
where
    T: Send + 'static,
    F: FnMut(StreamEvent) -> Fut,
    Fut: Future<Output = ()>,
{
    if !resources.owns(operation) {
        return Err(StructuredTaskError::ForeignOperation);
    }
    let mut cancel_rx = operation.cancel_receiver();
    if *cancel_rx.borrow() {
        return Err(StructuredTaskError::Cancelled);
    }
    let cancel = CancellationToken::from_receiver(cancel_rx.clone());
    let execution = execution.clone();
    let (sink, mut events) = EventSink::channel();
    let task =
        tokio::spawn(async move { execution.run(&contract, messages, &sink, &cancel).await });
    operation.set_abort_handle(task.abort_handle());

    let mut stream_error = None;
    let mut cancelled = false;
    loop {
        tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    cancelled = true;
                    break;
                }
            }
            event = events.recv() => match event {
                Some(StreamEvent::Cancelled) => {
                    cancelled = true;
                    break;
                }
                Some(StreamEvent::Error(message)) => {
                    stream_error = Some(message);
                    break;
                }
                Some(event) => deliver(event).await,
                None => break,
            }
        }
    }

    if *cancel_rx.borrow() || cancelled || stream_error.is_some() {
        let exit =
            abort_and_reap_with_deadline(operation_id, "structured-stream", deadline, task).await;
        if exit == TaskExit::Confirmed {
            operation.clear_abort_handle();
        }
        if *cancel_rx.borrow() || cancelled {
            return Err(StructuredTaskError::Cancelled);
        }
        return match stream_error {
            Some(message) => Err(StructuredTaskError::Stream(message)),
            None => Err(StructuredTaskError::Orphaned),
        };
    }

    let completion = await_with_deadline(operation_id, "structured-stream", deadline, task).await;
    if !matches!(completion, Awaited::Orphaned) {
        operation.clear_abort_handle();
    }
    if *cancel_rx.borrow() {
        return Err(StructuredTaskError::Cancelled);
    }
    match completion {
        Awaited::Joined(Ok(Ok(output))) => Ok(output),
        Awaited::Joined(Ok(Err(StructuredError::Cancelled))) => Err(StructuredTaskError::Cancelled),
        Awaited::Joined(Ok(Err(error))) => Err(StructuredTaskError::Request(error)),
        Awaited::Joined(Err(error)) => Err(StructuredTaskError::Task(error.to_string())),
        Awaited::Orphaned => Err(StructuredTaskError::Orphaned),
    }
}
