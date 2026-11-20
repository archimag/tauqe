use super::*;
use tauqe_core::config::AppConfig;
use tauqe_core::context::{ContextAccess, ContextLayer};
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
fn interactive() -> InteractiveExecutionRequest {
    InteractiveExecutionRequest {
        capabilities: tauqe_core::control::repl_capabilities(None),
        prompt: "Current request".into(),
        focus: None,
    }
}
async fn dialogue(app: &Application, operation: &OperationGuard) -> DialogueExecution {
    ExecutionFactory::bind(app, operation)
        .await
        .unwrap()
        .for_dialogue()
        .await
        .unwrap()
}
async fn step(app: &Application, operation: &OperationGuard, task: &str) -> PreparedSingleStep {
    app.prepare_single_step(
        operation,
        SingleStepRequest {
            selection: Some(ModelSelection::Role(ModelRole::Coder)),
            execution: None,
            reasoning_effort: Some(ReasoningEffort::High),
            session_id: Some("step-session".into()),
            capabilities: tauqe_core::control::turn::turn_capability_expr(),
            history: None,
            task: task.into(),
            context: app.context_for_turn().await,
        },
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn interactive_launch_runs_headlessly_and_keeps_provider_inputs_and_event_lifetime() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let operation = Arc::new(app.try_begin_operation().unwrap());
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        r#"{"message":"Interactive reply","action":{"kind":"answer"}}"#,
    )]));
    app.test_set_provider(provider.clone()).await;
    let bound = dialogue(&app, &operation).await;
    let mut started = app
        .start_interactive_execution(operation.clone(), interactive(), bound)
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
    assert_eq!(
        requests[0].session_id,
        Some(format!("tauqe-repl-{}", app.session_uuid().await))
    );
    assert_eq!(
        requests[0].reasoning_effort,
        app.resolve_turn_model(
            None,
            None,
            crate::application::turn_setup::TurnMode::Interactive
        )
        .await
        .effort
    );
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
    app.test_set_provider(provider.clone()).await;
    assert!(ExecutionFactory::bind(&app, &foreign).await.is_err());
    assert!(app
        .prepare_single_step(
            &foreign,
            SingleStepRequest {
                selection: None,
                execution: None,
                reasoning_effort: None,
                session_id: None,
                capabilities: tauqe_core::control::turn::turn_capability_expr(),
                history: None,
                task: "Execute the declared task".into(),
                context: app.context_for_turn().await,
            }
        )
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
    app.test_set_provider(provider.clone()).await;
    let prepared = step(&app, &operation, "Execute the declared task").await;
    let (sink, mut events) = EventSink::channel();
    let report = app
        .execute_prepared_single_step(&operation, "headless-step", prepared, sink)
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
    app.test_set_provider(provider.clone()).await;
    let prepared = step(&app, &operation, "Execute the declared task").await;
    let other = Application::default();
    let other_operation = other.try_begin_operation().unwrap();
    assert!(matches!(
        other
            .execute_prepared_single_step(
                &other_operation,
                "other-session",
                prepared,
                EventSink::noop()
            )
            .await,
        StepLoopReport::Failed(_)
    ));
    let prepared = step(&app, &operation, "Execute the declared task").await;
    drop(operation);
    let replacement = app.try_begin_operation().unwrap();
    assert!(matches!(
        app.execute_prepared_single_step(&replacement, "replacement", prepared, EventSink::noop())
            .await,
        StepLoopReport::Failed(_)
    ));
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn failed_step_retains_research_without_dialogue_history() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("helper.rs"), "helper source\n").unwrap();
    tauqe_core::git::cmd::stage_files(root.path(), &["helper.rs".into()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "Add helper", false).unwrap();
    let app = application(root.path());
    app.record_history_turn("Dialogue-only history marker".into(), Vec::new())
        .await
        .unwrap();
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(serde_json::json!({
            "message": "Read helper",
            "action": {"kind": "research", "context_requests": [{"path": "helper.rs", "access": "read_only"}]}
        }).to_string()),
        MockResponse::text(serde_json::json!({
            "message": "Cannot proceed",
            "action": {"kind": "blocked", "blocked": {"reason": "missing prerequisite", "details": "", "is_blocked": true}}
        }).to_string()),
    ]));
    app.test_set_provider(provider.clone()).await;
    let operation = app.try_begin_operation().unwrap();
    let prepared = app
        .prepare_single_step(
            &operation,
            SingleStepRequest {
                selection: None,
                execution: None,
                reasoning_effort: None,
                session_id: None,
                capabilities: tauqe_core::control::turn::turn_capability_expr(),
                history: None,
                task: "Use helper".into(),
                context: app.context_for_turn().await,
            },
        )
        .await
        .unwrap();
    let report = app
        .execute_prepared_single_step(&operation, "research-step", prepared, EventSink::noop())
        .await;
    assert!(matches!(
        report,
        StepLoopReport::Turn(LoopResult::Diverged(_))
    ));
    assert!(app
        .context_state()
        .await
        .items
        .iter()
        .any(|item| item.path == "helper.rs" && item.layer == ContextLayer::Auto));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request
        .messages
        .iter()
        .all(|message| !message.content.contains("Dialogue-only history marker"))));
}

#[tokio::test]
async fn successful_step_clears_transient_context_and_publishes_snapshot() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.rs"), "original\n").unwrap();
    tauqe_core::git::cmd::stage_files(root.path(), &["source.rs".into()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "Add source", false).unwrap();
    let app = application(root.path());
    app.test_mutate_context(|context| context.add_auto_file("source.rs", ContextAccess::Editable))
        .await
        .unwrap()
        .unwrap();
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(serde_json::json!({
        "message": "Updated source",
        "action": {"kind": "edit", "patch": "*** Begin Patch\n*** Update File: source.rs\n@@\n-original\n+updated\n*** End Patch", "commit_message": "fix(source): update source", "reason": ""}
    }).to_string())]));
    app.test_set_provider(provider).await;
    let operation = app.try_begin_operation().unwrap();
    let prepared = app
        .prepare_single_step(
            &operation,
            SingleStepRequest {
                selection: None,
                execution: None,
                reasoning_effort: None,
                session_id: None,
                capabilities: tauqe_core::control::turn::turn_capability_expr(),
                history: None,
                task: "Update source".into(),
                context: app.context_for_turn().await,
            },
        )
        .await
        .unwrap();
    let (sink, mut events) = EventSink::channel();
    let report = app
        .execute_prepared_single_step(&operation, "edit-step", prepared, sink)
        .await;
    assert!(matches!(
        report,
        StepLoopReport::Turn(LoopResult::Success(_))
    ));
    assert!(app.context_state().await.items.is_empty());
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(
        |event| matches!(event, StreamEvent::ContextChanged(state) if state.items.is_empty())
    ));
}

#[tokio::test]
async fn neighboring_step_units_do_not_inherit_failed_staging() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.rs"), "original\n").unwrap();
    tauqe_core::git::cmd::stage_files(root.path(), &["source.rs".into()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "Add source", false).unwrap();
    let app = application(root.path());
    app.test_mutate_context(|context| context.add_file("source.rs", ContextAccess::Editable))
        .await
        .unwrap()
        .unwrap();
    let response = |old: &str| {
        serde_json::json!({
        "message": "Update source",
        "action": {"kind": "edit", "patch": format!("*** Begin Patch\n*** Update File: source.rs\n@@\n-{old}\n+updated\n*** End Patch"), "commit_message": "fix(source): update source", "reason": ""}
    }).to_string()
    };
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(response("absent")),
        MockResponse::text(serde_json::json!({
            "message": "Cannot repair",
            "action": {"kind": "blocked", "blocked": {"reason": "cannot repair", "details": "", "is_blocked": true}}
        }).to_string()),
        MockResponse::text(response("original")),
    ]));
    app.test_set_provider(provider.clone()).await;
    let operation = app.try_begin_operation().unwrap();
    for (index, expected_success) in [false, true].into_iter().enumerate() {
        let prepared = app
            .prepare_single_step(
                &operation,
                SingleStepRequest {
                    selection: None,
                    execution: None,
                    reasoning_effort: None,
                    session_id: None,
                    capabilities: tauqe_core::control::turn::turn_capability_expr(),
                    history: None,
                    task: format!("Attempt {}", index + 1),
                    context: app.context_for_turn().await,
                },
            )
            .await
            .unwrap();
        let report = app
            .execute_prepared_single_step(&operation, "staging-step", prepared, EventSink::noop())
            .await;
        assert_eq!(
            matches!(report, StepLoopReport::Turn(LoopResult::Success(_))),
            expected_success
        );
    }
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(
        std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
        "updated\n"
    );
}

#[tokio::test]
async fn dialogue_continuation_uses_its_new_capability_grant() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let operation = app.try_begin_operation().unwrap();
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text("First reply"),
        MockResponse::text("Second reply"),
    ]));
    app.test_set_provider(provider.clone()).await;
    let mut dialogue = dialogue(&app, &operation).await;
    let mut context = app.context_for_turn().await;
    let first = tauqe_core::control::repl_capabilities(None);
    dialogue
        .execute(
            root.path(),
            &mut context,
            &CancellationToken::new(),
            &EventSink::noop(),
            "First task".into(),
            first,
            None,
        )
        .await
        .unwrap();
    let narrowed = CapabilityTree::set([CapabilityTree::leaf(
        tauqe_core::llm::LlmCapability::TextMessage,
    )]);
    dialogue
        .continue_round(
            root.path(),
            &mut context,
            &CancellationToken::new(),
            &EventSink::noop(),
            narrowed,
            None,
            "Continue with text only",
        )
        .await
        .unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].response_format, requests[1].response_format);
    let second = serde_json::to_string(&requests[1].response_format).unwrap();
    assert!(!second.contains("plan_update"));
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
            app.test_set_provider(provider.clone()).await;
            let prepared = step(&app, &operation, TASK).await;
            let result = app
                .execute_prepared_single_step(
                    &operation,
                    "prefix-test",
                    prepared,
                    EventSink::noop(),
                )
                .await;
            assert!(matches!(
                result,
                StepLoopReport::Turn(LoopResult::Success(_))
            ));
        } else {
            let mut request = interactive();
            request.prompt = TASK.into();
            app.test_set_provider(provider.clone()).await;
            let bound = dialogue(&app, &operation).await;
            let mut started = app
                .start_interactive_execution(operation, request, bound)
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
