use super::lifecycle::{self, LifecycleServices};
use super::settlement::{classify_task, interrupt, InterruptionCause, TaskSettlement};
use super::*;
use crate::api::{Application, ApplicationEvent, ApplicationObserver, InteractiveTaskOutcome};
use crate::application::discussion_prompt::{PreparedModelAsk, ResolvedDiscussion};
use crate::application::ExecutionFactory;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::config::AppConfig;
use tauqe_core::models::{ModelRole, ProviderKind};
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::plan::PlanRejectionKind;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::review::storage::ReviewStorage;

struct NoopObserver;

impl ApplicationObserver for NoopObserver {
    fn publish(&self, _: &str, _: ApplicationEvent) {}
}

async fn execute(
    root: &Path,
    responses: Vec<MockResponse>,
    discussion: Option<ResolvedDiscussion>,
    rounds: usize,
) -> (
    Result<ReplResult, ExecutionError>,
    Vec<tauqe_core::providers::mock::MockRequest>,
) {
    let mut config: AppConfig =
        toml::from_str(&format!("[repl]\nmax_turn_rounds = {rounds}\n")).unwrap();
    config.context.include_repo_map = false;
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let app = Application::for_repository(config, root.to_path_buf(), history);
    let provider = Arc::new(MockLlmProvider::scripted(responses));
    app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
        .await;
    let operation = Arc::new(app.try_begin_operation().unwrap());
    let factory = ExecutionFactory::bind(&app, &operation).await.unwrap();
    let model = factory.resolve_model(factory.selection().clone(), ModelRole::REPL);
    let max_turn_rounds =
        tauqe_core::workflow::WorkflowOptions::from_app_config(&model.config).max_turn_rounds();
    let mut executor = factory
        .bind_prompt(
            &model.model,
            model.effort,
            model.role,
            None,
            model.config.repl.patch_strategy(),
        )
        .await
        .unwrap();
    let scope = app.execution_session().prepare_scope();
    let binding = scope.binding();
    executor.bind_usage(binding.clone()).unwrap();
    let config = Arc::new(model.config.clone());
    let policy = factory.apply_policy(model.config.repl.workflow);
    let prepared = PreparedModelAsk {
        utterance: "Discuss the change".into(),
        discussion,
        focus_update: None,
    };
    let services = LifecycleServices {
        context: app.context_session().clone(),
        interactive: app.interactive_execution().clone(),
        execution: app.execution_session(),
        history: app.history_session().clone(),
    };
    let context = services.context.clone();
    // The helper runs the task directly, so activate the same accounting
    // binding before history preparation or model execution.
    let (result, _) = scope
        .run(
            &context,
            operation.publication_id(),
            |_, _| {},
            async move {
                let started = lifecycle::start_task(
                    &services,
                    operation.clone(),
                    prepared,
                    max_turn_rounds,
                    config,
                    binding,
                    executor,
                    policy,
                    EventSink::noop(),
                    Arc::new(NoopObserver),
                )
                .await
                .unwrap();
                let report = started.join().await;
                report
                    .preparation
                    .complete(
                        &operation,
                        crate::application::interactive_history::CompleteTurn {
                            rounds: report.rounds,
                            source: report.source,
                            response: String::new(),
                            edit: None,
                        },
                    )
                    .await
                    .unwrap();
                report.task.unwrap().result
            },
            |_| {},
            |outcome| async move { outcome },
        )
        .await
        .unwrap();
    let tauqe_core::execution::task::TaskOutcome::Completed(result) = result else {
        panic!("interactive test task failed");
    };
    (result, provider.requests())
}

fn plan_payload(valid: bool) -> String {
    let step = serde_json::json!({"id":"1","title":"Step","files":["src/step.rs"]});
    serde_json::json!({
        "message": "Plan response",
        "action": {"kind": "plan", "plan_create": {
            "action": "save", "id": "corrected", "title": "Corrected",
            "concept": [{"id":"c-1","title":"Invariant"}],
            "items": if valid { vec![step] } else { vec![step.clone(), step] }
        }, "commit_message":"create corrected plan", "reason":""}
    })
    .to_string()
}

#[tokio::test]
async fn correction_uses_the_same_round_budget_and_persists_only_valid_plans() {
    for (rounds, expected_applied, calls) in [(2, true, 2), (1, false, 1)] {
        let root = tempfile::tempdir().unwrap();
        tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
        let (result, requests) = execute(
            root.path(),
            vec![
                MockResponse::text(plan_payload(false)),
                MockResponse::text(plan_payload(true)),
            ],
            None,
            rounds,
        )
        .await;
        assert_eq!(requests.len(), calls);
        let result = document(result);
        assert_eq!(
            tauqe_core::plan::storage::PlanStorage::load_plan(root.path(), "corrected")
                .unwrap()
                .is_some(),
            expected_applied
        );
        if expected_applied {
            assert!(
                matches!(result.disposition, DocumentDisposition::Saved(ReplText::Accepted { text, .. }) if text == "Plan response")
            );
            assert!(result.plan_rejections.is_empty());
            assert!(requests[1]
                .messages
                .iter()
                .any(|message| message.content.contains("[Plan Correction 1]")
                    && message.content.contains("Duplicate step ID")));
        } else {
            assert!(
                matches!(result.disposition, DocumentDisposition::Refused(error) if error.contains("budget was exhausted"))
            );
            assert_eq!(
                result.plan_rejections[0].kind,
                PlanRejectionKind::PayloadDefect
            );
            assert!(result.plan_rejections[0]
                .message
                .contains("Duplicate step ID"));
        }
    }
}

fn document(result: Result<ReplResult, ExecutionError>) -> DocumentResult {
    let ReplResult::Document(document) = result.unwrap() else {
        panic!("expected a document result");
    };
    document
}

#[tokio::test]
async fn budget_before_first_round_is_an_environment_answer() {
    let root = tempfile::tempdir().unwrap();
    let (result, requests) = execute(root.path(), vec![], None, 0).await;
    assert!(requests.is_empty());
    assert!(
        matches!(result, Ok(ReplResult::Answer(ReplText::Environment(text)))
        if text.contains("Round Budget Reached") && text.contains("0 provider call(s)"))
    );
}

#[tokio::test]
async fn budget_after_research_is_an_error_not_an_environment_answer() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
    let (result, requests) = execute(
        root.path(),
        vec![MockResponse::text(
            r#"{"message":"Research accepted.","action":{"kind":"research","context_requests":[{"path":"source.txt","access":"read_only"}]}}"#,
        )],
        None,
        1,
    )
    .await;
    assert_eq!(requests.len(), 1);
    assert!(matches!(result, Err(ExecutionError::Other(error))
        if error.contains("budget was exhausted after 1 call(s)")));
}

#[tokio::test]
async fn review_decisions_address_only_the_frozen_focus() {
    for (item_id, corrupt) in [(7, false), (9, false), (7, true)] {
        check_review_decision(item_id, corrupt).await;
    }
}

async fn check_review_decision(item_id: u32, corrupt: bool) {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    for id in ["focused", "other"] {
        let session = tauqe_core::review::ReviewSession {
            id: id.into(),
            title: id.into(),
            items: vec![serde_json::from_value(serde_json::json!({
                "id": if id == "focused" { 7 } else { item_id }, "title":"Race"
            }))
            .unwrap()],
            ..Default::default()
        };
        ReviewStorage::save_session(root.path(), &session).unwrap();
    }
    let discussion = Application::default()
        .prepare_model_ask(
            root.path(),
            crate::api::ModelAskInput::Discussion {
                target: DiscussionTarget::Review {
                    id: "focused".into(),
                },
                focused_items: vec![],
                comment: String::new(),
            },
        )
        .await
        .unwrap()
        .discussion;
    assert!(discussion.is_some());
    let path = root.path().join(".tauqe/reviews/focused.json");
    if corrupt {
        std::fs::write(&path, "not JSON").unwrap();
    }
    let original = std::fs::read(&path).unwrap();
    let other_path = root.path().join(".tauqe/reviews/other.json");
    let other_original = std::fs::read(&other_path).unwrap();
    let response = serde_json::json!({
        "message":"Decision recorded.",
        "action":{"kind":"review","review_update":{"item_id":item_id,"group_id":null,
            "status":"rejected","note":"Already verified","recommendations":[]},
            "commit_message":"","reason":""}
    });
    let (result, requests) = execute(
        root.path(),
        vec![MockResponse::text(response.to_string())],
        discussion,
        2,
    )
    .await;
    assert_eq!(requests.len(), 1);
    let result = document(result);
    assert_eq!(std::fs::read(other_path).unwrap(), other_original);
    if corrupt || item_id != 7 {
        let diagnostic = match result.disposition {
            DocumentDisposition::StorageFailed(error) if corrupt => error,
            DocumentDisposition::Refused(error) if !corrupt => error,
            _ => panic!("expected the appropriate review failure"),
        };
        assert!(
            diagnostic.contains(if corrupt {
                "Failed to parse review JSON"
            } else {
                "finding not found in the review focus"
            }),
            "{diagnostic}"
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
        return;
    }
    assert!(
        matches!(result.disposition, DocumentDisposition::Saved(ReplText::Accepted { text, .. }) if text == "Decision recorded.")
    );
    assert_eq!(
        ReviewStorage::load_review(root.path(), "focused")
            .unwrap()
            .unwrap()
            .find_item(7)
            .unwrap()
            .status,
        tauqe_core::review::IssueStatus::Rejected
    );
    assert_eq!(
        ReviewStorage::load_review(root.path(), "other")
            .unwrap()
            .unwrap()
            .find_item(7)
            .unwrap()
            .status,
        tauqe_core::review::IssueStatus::Discussion
    );
}

#[tokio::test]
async fn a_free_dialogue_cannot_update_an_existing_plan() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    let (created, _) = execute(
        root.path(),
        vec![MockResponse::text(plan_payload(true))],
        None,
        2,
    )
    .await;
    assert!(matches!(
        document(created).disposition,
        DocumentDisposition::Saved(_)
    ));
    let path = PlanStorage::plans_dir(root.path()).join("corrected.json");
    let original = std::fs::read(&path).unwrap();
    let (refused, requests) = execute(
        root.path(),
        vec![MockResponse::text(
            plan_payload(true).replace("Corrected", "Replacement"),
        )],
        None,
        2,
    )
    .await;
    assert_eq!(requests.len(), 1);
    let refused = document(refused);
    assert!(
        matches!(refused.disposition, DocumentDisposition::Refused(error) if error.contains("Plan proposal refused"))
    );
    assert_eq!(
        refused.plan_rejections[0].kind,
        PlanRejectionKind::ModeForbidden
    );
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[tokio::test]
async fn a_storage_failure_is_terminal_and_leaves_no_document_saved() {
    for (conflict, diagnostic) in [
        ("", "Failed to read plans directory"),
        ("corrected.json", "Is a directory"),
        ("active", "Is a directory"),
    ] {
        let root = tempfile::tempdir().unwrap();
        tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
        let plans = PlanStorage::plans_dir(root.path());
        if conflict.is_empty() {
            std::fs::create_dir_all(plans.parent().unwrap()).unwrap();
            std::fs::write(&plans, "not a directory").unwrap();
        } else {
            std::fs::create_dir_all(plans.join(conflict)).unwrap();
        }
        let (result, requests) = execute(
            root.path(),
            vec![
                MockResponse::text(plan_payload(true)),
                MockResponse::text(plan_payload(true)),
            ],
            None,
            2,
        )
        .await;
        assert_eq!(requests.len(), 1);
        let TaskSettlement::Returned(task) = classify_task(
            Ok(InteractiveTaskOutcome {
                result,
                context: Default::default(),
            }),
            false,
            None,
            None,
            None,
        ) else {
            panic!("a reported plan storage failure is a task result, not an interruption");
        };
        let DocumentDisposition::StorageFailed(error) = document(task.result).disposition else {
            panic!("a storage failure must not be treated as a correctable refusal");
        };
        assert!(error.contains(diagnostic), "{error}");
        assert!(!error.contains("already saved"));
        let document = plans.join("corrected.json");
        assert!(!document.is_file());
    }
}

#[test]
fn a_finalized_edit_cannot_be_settled_as_a_text_answer() {
    let edit = EditOutcome {
        applied: true,
        commit_hash: Some("1234567".into()),
        error: None,
        changed_files: vec!["source.txt".into()],
        verification: None,
    };
    let answer = || {
        Ok(InteractiveTaskOutcome {
            result: Ok(ReplResult::Answer(ReplText::Environment("Done".into()))),
            context: Default::default(),
        })
    };
    assert!(matches!(
        classify_task(answer(), false, None, None, None),
        TaskSettlement::Returned(_)
    ));
    assert!(matches!(
        classify_task(answer(), false, None, None, Some(&edit)),
        TaskSettlement::Interrupted {
            cause: InterruptionCause::MissingEdit,
            diagnostic,
        } if diagnostic.contains("did not return the finalized edit")
    ));
    assert!(matches!(
        classify_task(answer(), true, Some("stream failed"), Some("worker failed"), Some(&edit)),
        TaskSettlement::Interrupted {
            cause: InterruptionCause::Cancelled,
            diagnostic,
        } if diagnostic.contains("stream failed") && diagnostic.contains("worker failed")
    ));
}

#[test]
fn interruption_matrix_preserves_receipts_and_uncertain_recovery() {
    for (cause, worker_error, confirmed, unknown, expect_diagnostic) in [
        (InterruptionCause::Cancelled, false, false, false, false),
        (InterruptionCause::Cancelled, true, false, false, true),
        (InterruptionCause::Cancelled, false, true, false, true),
        (InterruptionCause::Cancelled, false, false, true, true),
        (InterruptionCause::TaskFailed, true, true, true, true),
    ] {
        let documents = documents::DocumentChanges {
            reviews: if confirmed {
                vec![tauqe_core::review::ReviewSession {
                    id: "focused".into(),
                    ..Default::default()
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let diagnostic = if worker_error {
            "document worker failed".into()
        } else {
            String::new()
        };
        let result = interrupt(cause, diagnostic, vec![], None, &documents, None, unknown);
        assert_eq!(result.cause, cause);
        assert_eq!(!result.diagnostic.is_empty(), expect_diagnostic);
        let history = result.history;
        assert_eq!(history.cancelled, cause == InterruptionCause::Cancelled);
        assert_eq!(history.confirmed_documents, confirmed);
        assert_eq!(!history.diagnostic.is_empty(), expect_diagnostic);
        if unknown {
            assert!(history.diagnostic.contains("recovery is unconfirmed"));
        }
        if confirmed {
            assert!(history.reason.contains("focused"));
        }
    }
}
