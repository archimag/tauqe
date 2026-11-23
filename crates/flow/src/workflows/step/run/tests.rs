use super::*;
use crate::api::{Application, ApplicationEvent, DiscussionTarget, ModelAskInput};
use crate::application::ExecutionFactory;
use crate::workflows::step::completion::classify_step_attempt;
use tauqe_core::config::AppConfig;
use tauqe_core::context::{ContextAccess, ContextLayer};
use tauqe_core::llm::step::turn_capability_expr;
use tauqe_core::models::ModelRole;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::implementation::StepTurnResult;
use tauqe_core::workflow::WorkflowKind;
use tokio::sync::mpsc;
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
        let tree = tauqe_core::llm::repl::repl_capabilities(focus.as_ref());
        assert_eq!(turn_prompt_protocol(tree.clone()).capability_expr(), tree);
    }
    let step_tree = turn_capability_expr();
    assert_eq!(
        turn_prompt_protocol(step_tree.clone()).capability_expr(),
        step_tree
    );
}
fn application(root: &std::path::Path) -> Application {
    let (history, _) = mpsc::unbounded_channel();
    Application::for_repository(config(), root.to_path_buf(), history)
}
struct Observer(mpsc::UnboundedSender<ApplicationEvent>);

impl ApplicationObserver for Observer {
    fn publish(&self, _: &str, event: ApplicationEvent) {
        let _ = self.0.send(event);
    }
}
fn step_result(report: StepLoopReport) -> StepTurnResult {
    let StepLoopReport::Turn(report) = report else {
        panic!("expected a settled attempt");
    };
    classify_step_attempt(&report).result
}

async fn step(app: &Application, operation: &OperationGuard, task: &str) -> PreparedSingleStep {
    app.step_resources()
        .prepare_single_step(
            operation,
            SingleStepRequest {
                selection: Some(ModelSelection::Role(ModelRole::Coder)),
                execution: PlanExecutionEnvironment::new(app.configuration().await, operation),
                reasoning_effort: Some(ReasoningEffort::High),
                session_id: Some("step-session".into()),
                capabilities: turn_capability_expr(),
                task: task.into(),
                context: app.context_for_turn().await,
            },
        )
        .await
        .unwrap()
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
        .step_resources()
        .prepare_single_step(
            &foreign,
            SingleStepRequest {
                selection: None,
                execution: PlanExecutionEnvironment::new(app.configuration().await, &foreign),
                reasoning_effort: None,
                session_id: None,
                capabilities: turn_capability_expr(),
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
        r#"{"message":"","action":{"kind":"edit","patch":"","commit_message":"","error_message":"","reason":"Already done"}}"#,
    )]));
    app.test_set_provider(provider.clone()).await;
    let prepared = step(&app, &operation, "Execute the declared task").await;
    let (sink, mut events) = EventSink::channel();
    let report = app
        .step_resources()
        .execute_prepared_single_step(&operation, "headless-step", prepared, sink)
        .await;
    assert!(matches!(
        step_result(report),
        StepTurnResult::Success { .. }
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
            .step_resources()
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
        app.step_resources()
            .execute_prepared_single_step(&replacement, "replacement", prepared, EventSink::noop())
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
        .step_resources()
        .prepare_single_step(
            &operation,
            SingleStepRequest {
                selection: None,
                execution: PlanExecutionEnvironment::new(app.configuration().await, &operation),
                reasoning_effort: None,
                session_id: None,
                capabilities: turn_capability_expr(),
                task: "Use helper".into(),
                context: app.context_for_turn().await,
            },
        )
        .await
        .unwrap();
    let report = app
        .step_resources()
        .execute_prepared_single_step(&operation, "research-step", prepared, EventSink::noop())
        .await;
    assert!(matches!(
        step_result(report),
        StepTurnResult::Blocked(reason) if reason == "missing prerequisite"
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
    let head = || tauqe_core::git::cmd::run_git(Some(root.path()), &["rev-parse", "HEAD"]).unwrap();
    let initial_head = head();
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
        .step_resources()
        .prepare_single_step(
            &operation,
            SingleStepRequest {
                selection: None,
                execution: PlanExecutionEnvironment::new(app.configuration().await, &operation),
                reasoning_effort: None,
                session_id: None,
                capabilities: turn_capability_expr(),
                task: "Update source".into(),
                context: app.context_for_turn().await,
            },
        )
        .await
        .unwrap();
    let (sink, mut events) = EventSink::channel();
    let report = app
        .step_resources()
        .execute_prepared_single_step(&operation, "edit-step", prepared, sink)
        .await;
    assert!(matches!(
        &report,
        StepLoopReport::Turn(report) if matches!(classify_step_attempt(report).result, StepTurnResult::Success { .. })
    ));
    assert!(app.context_state().await.items.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
        "updated\n"
    );
    assert_eq!(
        head(),
        initial_head,
        "direct application does not advance HEAD"
    );
    let completion = classify_step_edit_completion(&report);
    assert_eq!(completion.outcome.changed_files, ["source.rs"]);
    assert!(completion.outcome.commit_hash.is_none());
    assert!(completion.outcome.error.is_none());
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(
        |event| matches!(event, StreamEvent::ContextChanged(state) if state.items.is_empty())
    ));
}

#[tokio::test]
async fn isolated_step_distinguishes_non_answers_from_round_budget_exhaustion() {
    use crate::workflows::step::completion::StepEditDisposition;
    use tauqe_core::providers::types::TurnRequestReason;

    for (responses, expected, rounds) in [
        (
            vec!["The work looks complete."],
            StepEditDisposition::Completed,
            vec![(1, None)],
        ),
        (
            vec![r#"{"message":"The work looks complete.","action":{"kind":"answer"}}"#],
            StepEditDisposition::Completed,
            vec![(1, None)],
        ),
        (
            vec!["", ""],
            StepEditDisposition::Refused,
            vec![(1, None), (2, Some(TurnRequestReason::EmptyResponseRetry))],
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.repl =
            toml::from_str::<AppConfig>("[repl]\nworkflow = 'naive'\nmax_turn_rounds = 2\n")
                .unwrap()
                .repl;
        let (history, _) = mpsc::unbounded_channel();
        let app = Application::for_repository(config, root.path().to_path_buf(), history);
        let provider = Arc::new(MockLlmProvider::scripted(
            responses.into_iter().map(MockResponse::text),
        ));
        app.test_set_provider(provider.clone()).await;
        let operation = app.try_begin_operation().unwrap();
        let prepared = step(&app, &operation, "Implement the step").await;
        let (sink, mut events) = EventSink::channel();
        let report = app
            .step_resources()
            .execute_prepared_single_step(&operation, "non-answer", prepared, sink)
            .await;
        let completion = classify_step_edit_completion(&report);
        assert_eq!(completion.disposition, expected);
        assert!(!completion.outcome.applied);
        assert!(completion.outcome.changed_files.is_empty());
        assert_eq!(provider.requests().len(), rounds.len());
        let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                StreamEvent::TurnRoundStarted {
                    round,
                    max_rounds,
                    reason,
                } => {
                    assert_eq!(max_rounds, Some(2));
                    Some((round, reason))
                }
                _ => None,
            })
            .collect();
        assert_eq!(observed, rounds);
        let StepLoopReport::Turn(report) = report else {
            panic!("expected a settled attempt")
        };
        assert!(matches!(
            classify_step_attempt(&report).result,
            StepTurnResult::Failed(_)
        ));
        match report.outcome {
            Err(RoundError::BudgetExhausted { spent: 2 }) => {
                assert_eq!(expected, StepEditDisposition::Refused);
            }
            Ok(TurnOutcome::ProtocolDefect(_) | TurnOutcome::Settled(_)) => {
                assert_eq!(expected, StepEditDisposition::Completed);
            }
            _ => panic!("a non-answer must not complete the step"),
        }
    }
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
            .step_resources()
            .prepare_single_step(
                &operation,
                SingleStepRequest {
                    selection: None,
                    execution: PlanExecutionEnvironment::new(app.configuration().await, &operation),
                    reasoning_effort: None,
                    session_id: None,
                    capabilities: turn_capability_expr(),
                    task: format!("Attempt {}", index + 1),
                    context: app.context_for_turn().await,
                },
            )
            .await
            .unwrap();
        let report = app
            .step_resources()
            .execute_prepared_single_step(&operation, "staging-step", prepared, EventSink::noop())
            .await;
        assert_eq!(
            matches!(step_result(report), StepTurnResult::Success { .. }),
            expected_success
        );
    }
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].messages.starts_with(&requests[0].messages));
    assert!(requests[2].messages.iter().all(|message| {
        message.role != "assistant" && !message.content.contains("Rejected patch")
    }));
    assert_eq!(
        std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
        "updated\n"
    );
}

#[tokio::test]
async fn isolated_step_preserves_partial_staging_across_research() {
    use tauqe_tools::edits::anchor::encode_anchor;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    let paths = ["target.txt", "other.txt", "helper.txt"];
    for (path, text) in
        paths
            .into_iter()
            .zip(["original target", "original other", "repair reference"])
    {
        std::fs::write(root.path().join(path), format!("{text}\n")).unwrap();
    }
    tauqe_core::git::cmd::stage_files(root.path(), &paths.map(String::from)).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "Add sources", false).unwrap();

    let mut config = config();
    config.repl = toml::from_str::<AppConfig>(
        "[repl]\nworkflow = 'naive'\npatch_strategy = 'line_anchor'\nmax_turn_rounds = 4\n",
    )
    .unwrap()
    .repl;
    let (history, _) = mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.path().into(), history);
    for path in ["target.txt", "other.txt"] {
        app.test_mutate_context(move |context| context.add_file(path, ContextAccess::Editable))
            .await
            .unwrap()
            .unwrap();
    }

    let replacement = |path: &str, anchor: &str, content: &str| {
        serde_json::json!({
            "op": "replace", "path": path, "to": "", "start": anchor, "end": anchor,
            "content": content
        })
    };
    let edit = |changes: Vec<serde_json::Value>| {
        MockResponse::text(serde_json::json!({
            "message": "Update both files",
            "action": {"kind": "edit", "changes": changes, "commit_message": "fix: update both files"}
        }).to_string())
    };
    let target = encode_anchor("target.txt", 0);
    let other = encode_anchor("other.txt", 0);
    let provider = Arc::new(MockLlmProvider::scripted([
        edit(vec![
            replacement("target.txt", &encode_anchor("target.txt", 99), "new target"),
            replacement("other.txt", &other, "staged other"),
        ]),
        MockResponse::text(
            r#"{"message":"Read repair reference","action":{"kind":"research","context_requests":[{"path":"helper.txt","access":"read_only"}]}}"#,
        ),
        edit(vec![replacement("target.txt", &target, "new target")]),
    ]));
    app.test_set_provider(provider.clone()).await;
    let operation = app.try_begin_operation().unwrap();
    let prepared = step(&app, &operation, "Update both files").await;
    let report = app
        .step_resources()
        .execute_prepared_single_step(&operation, "partial-staging", prepared, EventSink::noop())
        .await;
    assert!(matches!(
        step_result(report),
        StepTurnResult::Success { .. }
    ));

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        let versions = &request.messages.last().unwrap().content;
        assert!(versions.starts_with("<current_versions>"));
        assert!(versions.contains(&format!("{other} | staged other")));
    }
    assert!(requests[2].messages.iter().any(|message| {
        message.content.starts_with("<context>") && message.content.contains("repair reference")
    }));
    for (path, expected) in [
        ("target.txt", "new target\n"),
        ("other.txt", "staged other\n"),
    ] {
        assert_eq!(
            std::fs::read_to_string(root.path().join(path)).unwrap(),
            expected
        );
    }
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
        app.test_set_provider(provider.clone()).await;
        if plan {
            let operation = app.try_begin_operation().unwrap();
            let prepared = step(&app, &operation, TASK).await;
            let result = app
                .step_resources()
                .execute_prepared_single_step(
                    &operation,
                    "prefix-test",
                    prepared,
                    EventSink::noop(),
                )
                .await;
            assert!(matches!(
                step_result(result),
                StepTurnResult::Success { .. }
            ));
        } else {
            let (events, mut received) = mpsc::unbounded_channel();
            app.ask(
                ModelAskInput::Prompt { text: TASK.into() },
                Arc::new(Observer(events)),
            )
            .await
            .unwrap();
            let mut finished = false;
            while let Some(event) = received.recv().await {
                match event {
                    ApplicationEvent::Result { completion, .. } => assert!(
                        completion.error.is_none(),
                        "{label}: {:?}",
                        completion.error
                    ),
                    ApplicationEvent::Finished(_) => finished = true,
                    _ => {}
                }
            }
            assert!(finished, "{label}");
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
