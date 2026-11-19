use super::*;
use tauqe_core::control::outcome::LoopResult;
use tauqe_core::models::ModelRole;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::workflow::WorkflowKind;
fn config() -> AppConfig {
    let mut config = AppConfig::default();
    config.repl.workflow = WorkflowKind::Naive;
    config.context.include_repo_map = false;
    config
}

#[test]
fn protocol_starts_with_the_declared_contour_tree() {
    for focus in [
        None,
        Some(DiscussionTarget::Plan { id: "plan".into() }),
        Some(DiscussionTarget::Review {
            id: "review".into(),
        }),
    ] {
        let tree = tauqe_core::control::repl_capabilities(focus.as_ref());
        assert_eq!(turn_prompt_protocol(tree.clone()).capability_expr(), tree);
    }
    let step_tree = tauqe_core::control::turn::turn_capability_expr();
    assert_eq!(
        turn_prompt_protocol(step_tree.clone()).capability_expr(),
        step_tree
    );
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
        capabilities: tauqe_core::control::repl_capabilities(None),
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

#[tokio::test]
async fn compare_repl_and_plan_request_prefixes() {
    const TASK: &str = "Replace the marker in sample.txt";
    const HISTORY: &str = "Earlier dialogue marker 8193";
    const SOURCE: &str = "Original source marker 2745";

    for (label, plan, with_history) in [
        ("REPL", false, false),
        ("REPL with history", false, true),
        ("plan step", true, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
        let source = format!("{SOURCE}\n{}", "Unchanged sample content.\n".repeat(200));
        std::fs::write(root.path().join("sample.txt"), source).unwrap();
        tauqe_core::git::cmd::stage_files(root.path(), &["sample.txt".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(root.path(), "Add fixture", false)
            .unwrap();
        let app = application(root.path());
        if with_history {
            app.record_history_turn(HISTORY.into(), Vec::new())
                .await
                .unwrap();
        }

        let research = serde_json::json!({
            "message": "Read the file",
            "action": {"kind":"research", "context_requests":[{"path":"sample.txt","access":"editable"}]}
        });
        let edit = |old: &str| {
            serde_json::json!({
                "message": "Update the marker",
                "action": {
                    "kind":"edit",
                    "patch": format!("*** Begin Patch\n*** Update File: sample.txt\n@@\n-{old}\n+Updated marker\n*** End Patch"),
                    "commit_message":"Update the marker",
                    "reason":""
                }
            })
        };
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(research.to_string()),
            MockResponse::text(edit("Absent marker").to_string()),
            MockResponse::text(edit(SOURCE).to_string()),
        ]));
        let operation = Arc::new(app.try_begin_operation().unwrap());
        if plan {
            let mut request = step(provider.clone());
            request.body = StepLoopBody::Single(TASK.into());
            let prepared = app
                .prepare_step_execution(&operation, request)
                .await
                .unwrap();
            let result = app
                .execute_prepared_step(&operation, "prefix-test", prepared, EventSink::noop())
                .await;
            assert!(matches!(
                result,
                StepLoopReport::Turn(LoopResult::Success(_))
            ));
        } else {
            let mut request = interactive(provider.clone());
            request.prompt = TASK.into();
            let mut started = app
                .start_interactive_execution(operation, request)
                .await
                .unwrap();
            while started.events.recv().await.is_some() {}
            let result = started.task.join("prefix-test").await.unwrap();
            assert!(
                matches!(result.result.as_ref().unwrap(), LoopResult::Success(_)),
                "{label}"
            );
        }
        assert!(std::fs::read_to_string(root.path().join("sample.txt"))
            .unwrap()
            .starts_with("Updated marker\n"));

        let requests = provider.requests();
        assert_eq!(requests.len(), 3, "{label}");
        let format = serde_json::to_value(&requests[0].response_format).unwrap();
        for request in &requests {
            assert_eq!(
                serde_json::to_value(&request.response_format).unwrap(),
                format,
                "{label}"
            );
            assert_eq!(
                request.messages.iter().any(|m| m.content.contains(HISTORY)),
                with_history && !plan,
                "{label}"
            );
            assert!(
                request.messages.iter().any(|m| m.content.contains(TASK)),
                "{label}"
            );
        }
        for (index, request) in requests.iter().enumerate() {
            let previous = index.checked_sub(1).map(|i| &requests[i].messages);
            let shared_messages = previous.map_or(0, |previous| {
                previous
                    .iter()
                    .zip(&request.messages)
                    .take_while(|(a, b)| a == b)
                    .count()
            });
            let content_bytes: usize = request.messages.iter().map(|m| m.content.len()).sum();
            let shared_bytes: usize = request.messages[..shared_messages]
                .iter()
                .map(|m| m.content.len())
                .sum();
            eprintln!("{label}, round {}: {content_bytes} content bytes, {shared_messages} identical prefix messages ({shared_bytes} content bytes)", index + 1);
        }
        for request in &requests[1..] {
            assert_eq!(
                request
                    .messages
                    .iter()
                    .map(|m| m.content.matches(SOURCE).count())
                    .sum::<usize>(),
                1,
                "{label}"
            );
        }
        for pair in requests.windows(2) {
            assert!(
                pair[1].messages.starts_with(&pair[0].messages),
                "{label}: a later round rewrote an earlier message"
            );
        }
    }
}
