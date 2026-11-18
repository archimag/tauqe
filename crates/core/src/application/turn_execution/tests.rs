use super::*;
use crate::control::outcome::LoopResult;
use crate::models::ModelRole;
use crate::providers::mock::{MockLlmProvider, MockResponse};
use crate::workflow::WorkflowKind;
fn config() -> AppConfig {
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config
}
fn application(root: &std::path::Path) -> Application {
    let (history, _) = mpsc::unbounded_channel();
    Application::for_repository(config(), root.to_path_buf(), history)
}
fn interactive(provider: Arc<MockLlmProvider>) -> InteractiveExecutionRequest {
    InteractiveExecutionRequest {
        provider,
        model: ModelRef::openrouter("mock/model"),
        config: config(),
        capabilities: crate::control::repl_capabilities(None),
        prompt: "Current request".into(),
        focus: None,
        session_id: Some("dialogue-session".into()),
        reasoning_effort: Some(ReasoningEffort::High),
    }
}
fn step(provider: Arc<MockLlmProvider>) -> StepExecutionRequest {
    StepExecutionRequest {
        provider,
        model: ModelRef::openrouter("mock/model"),
        config: config(),
        selection: ModelSelection::Role(ModelRole::Coder),
        reasoning_effort: Some(ReasoningEffort::High),
        marker: None,
        session_id: Some("step-session".into()),
        body: StepLoopBody::Single("Execute the declared task".into()),
    }
}
#[tokio::test]
async fn interactive_launch_runs_headlessly_and_keeps_provider_inputs_and_event_lifetime() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let operation = Arc::new(app.try_begin_operation().unwrap());
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        "Interactive reply",
    )]));
    let mut started = app
        .start_interactive_execution(operation.clone(), interactive(provider.clone()))
        .await
        .unwrap();
    let mut text = String::new();
    while let Some(event) = started.events.recv().await {
        if let StreamEvent::TextDelta(delta) = event {
            text.push_str(&delta);
        }
    }
    let outcome = started.task.join("headless-interactive").await.unwrap();
    assert_eq!(text, "Interactive reply");
    assert!(
        matches!(outcome.result,Ok(LoopResult::Success(success)) if success.value.message.as_deref()==Some("Interactive reply"))
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].session_id.as_deref(), Some("dialogue-session"));
    assert_eq!(requests[0].reasoning_effort, Some(ReasoningEffort::High));
    assert!(requests[0]
        .messages
        .iter()
        .any(|message| message.content.contains("Current request")));
    assert!(!app.abort_operation(operation.id()));
    assert_eq!(app.active_operation_id(), Some(operation.id()));
}
#[tokio::test]
async fn foreign_interactive_and_step_operations_do_not_reach_the_provider() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let other = Application::default();
    let foreign = Arc::new(other.try_begin_operation().unwrap());
    let provider = Arc::new(MockLlmProvider::new());
    assert!(matches!(
        app.start_interactive_execution(foreign.clone(), interactive(provider.clone()))
            .await,
        Err(InteractiveTaskError::OperationNotOwned)
    ));
    assert!(app
        .prepare_step_execution(&foreign, step(provider.clone()))
        .await
        .is_err());
    assert!(provider.requests().is_empty());
}
#[tokio::test]
async fn prepared_step_preserves_frozen_inputs_and_executes_through_supervision() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let operation = app.try_begin_operation().unwrap();
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"message":"","action":{"kind":"edit","changes":[],"commit_message":"","message":"","reason":"Already done"}}"#,
    )]));
    let prepared = app
        .prepare_step_execution(&operation, step(provider.clone()))
        .await
        .unwrap();
    let (sink, mut events) = EventSink::channel();
    let report = app
        .execute_prepared_step(&operation, "headless-step", prepared, sink)
        .await;
    assert!(matches!(
        report,
        StepLoopReport::Turn(LoopResult::Success(_))
    ));
    while events.recv().await.is_some() {}
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].session_id.as_deref(), Some("step-session"));
    assert_eq!(requests[0].reasoning_effort, Some(ReasoningEffort::High));
    assert!(requests[0]
        .messages
        .iter()
        .any(|message| message.content.contains("Execute the declared task")));
    assert!(!app.abort_operation(operation.id()));
    assert_eq!(app.active_operation_id(), Some(operation.id()));
}
#[tokio::test]
async fn prepared_step_cannot_be_executed_under_another_operation_or_session() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let operation = app.try_begin_operation().unwrap();
    let provider = Arc::new(MockLlmProvider::new());
    let prepared = app
        .prepare_step_execution(&operation, step(provider.clone()))
        .await
        .unwrap();
    let other = Application::default();
    let other_operation = other.try_begin_operation().unwrap();
    assert!(matches!(
        other
            .execute_prepared_step(
                &other_operation,
                "other-session",
                prepared,
                EventSink::noop()
            )
            .await,
        StepLoopReport::Failed(_)
    ));
    let prepared = app
        .prepare_step_execution(&operation, step(provider.clone()))
        .await
        .unwrap();
    drop(operation);
    let replacement = app.try_begin_operation().unwrap();
    assert!(matches!(
        app.execute_prepared_step(&replacement, "replacement", prepared, EventSink::noop())
            .await,
        StepLoopReport::Failed(_)
    ));
    assert!(provider.requests().is_empty());
}
