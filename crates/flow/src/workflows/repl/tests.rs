use super::*;
use crate::api::Application;
use crate::application::discussion_prompt::ResolvedDiscussion;
use crate::application::ExecutionFactory;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::config::AppConfig;
use tauqe_core::models::{ModelRole, ProviderKind};
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::review::storage::ReviewStorage;

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
    let executor = factory
        .bind_prompt(
            &model.model,
            model.effort,
            None,
            model.config.repl.patch_strategy(),
        )
        .await
        .unwrap();
    let policy = factory.apply_policy(model.config.repl.workflow);
    let prepared = PreparedModelAsk {
        utterance: "Discuss the change".into(),
        discussion,
        focus_update: None,
    };
    let mut started = start_task(&app, operation, prepared, model, executor, policy)
        .await
        .unwrap();
    while started.events.recv().await.is_some() {}
    let result = started.task.join().await.task.unwrap().result;
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
async fn forced_abort_drains_an_admitted_document_write_and_keeps_its_receipt() {
    use std::time::Duration;
    use tokio::sync::Notify;

    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    let plans = PlanStorage::plans_dir(root.path());
    std::fs::create_dir_all(plans.join("active")).unwrap();
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let app = Application::for_repository(Default::default(), root.path().to_path_buf(), history);
    let operation = Arc::new(app.try_begin_operation().unwrap());
    let admitted = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (documents, worker) = DocumentExecutor::spawn(
        root.path().to_path_buf(),
        CancellationToken::from_receiver(operation.cancel_receiver()),
        {
            let admitted = admitted.clone();
            let release = release.clone();
            move || {
                let admitted = admitted.clone();
                let release = release.clone();
                async move {
                    admitted.notify_one();
                    release.notified().await;
                }
            }
        },
    );
    let payload: serde_json::Value = serde_json::from_str(&plan_payload(true)).unwrap();
    let update = serde_json::from_value(payload["action"]["plan_create"].clone()).unwrap();
    let task = app
        .start_interactive_task(operation.clone(), move |context| async move {
            let result = documents
                .write(DocumentWrite::Plans {
                    updates: vec![update],
                    mode: PlanMutationMode::Repl,
                })
                .await;
            InteractiveTaskOutcome {
                result: result
                    .map(|_| ReplResult::Answer(ReplText::Environment("Write completed".into()))),
                context,
            }
        })
        .unwrap()
        .with_document_worker(worker);
    tokio::time::timeout(Duration::from_secs(10), admitted.notified())
        .await
        .unwrap();
    assert_eq!(app.cancel_active_operation(), Some(operation.id()));
    assert!(app.abort_operation(operation.id()));
    let mut joining = Box::pin(task.join());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut joining)
            .await
            .is_err()
    );
    assert!(!plans.join("corrected.json").exists());
    assert!(app.try_begin_operation().is_err());

    release.notify_one();
    let joined = tokio::time::timeout(Duration::from_secs(10), joining)
        .await
        .unwrap();
    assert!(
        matches!(joined.task, Err(InteractiveTaskError::Interrupted(cause)) if cause.is_cancelled())
    );
    assert!(joined.documents.join_error.is_none());
    let report = joined.documents.receipts;
    assert_eq!(report.len(), 1);
    let DocumentOutcome::Plans(outcomes) = report[0].as_ref() else {
        panic!("expected a plan write receipt");
    };
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].applied.len(), 1);
    assert_eq!(outcomes[0].applied[0].id, "corrected");
    assert!(outcomes[0]
        .storage_error
        .as_deref()
        .unwrap()
        .contains("active plan marker"));
    assert!(PlanStorage::load_plan(root.path(), "corrected")
        .unwrap()
        .is_some());
    assert!(app.try_begin_operation().is_err());
    drop(operation);
    assert!(app.try_begin_operation().is_ok());
}

#[tokio::test]
async fn document_worker_join_failure_preserves_previous_receipts() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    let mut first = true;
    let (documents, worker) = DocumentExecutor::spawn(
        root.path().to_path_buf(),
        CancellationToken::new(),
        move || {
            assert!(std::mem::take(&mut first), "simulated worker failure");
            std::future::ready(())
        },
    );
    let write = || {
        let payload: serde_json::Value = serde_json::from_str(&plan_payload(true)).unwrap();
        DocumentWrite::Plans {
            updates: vec![
                serde_json::from_value(payload["action"]["plan_create"].clone()).unwrap(),
            ],
            mode: PlanMutationMode::Repl,
        }
    };
    let first = documents.write(write()).await.unwrap();
    assert!(documents.write(write()).await.is_err());
    drop(documents);

    let settlement = worker.finish().await;
    assert!(settlement.join_error.unwrap().is_panic());
    assert_eq!(settlement.receipts.len(), 1);
    assert!(Arc::ptr_eq(&settlement.receipts[0], &first));
    let DocumentOutcome::Plans(outcomes) = first.as_ref() else {
        panic!("expected a plan write receipt");
    };
    assert_eq!(outcomes[0].applied[0].id, "corrected");
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
async fn a_storage_failure_is_terminal_and_reports_any_document_already_saved() {
    for (conflict, diagnostic, saved) in [
        ("", "Failed to read plans directory", false),
        ("corrected.json", "Failed to write plan document", false),
        ("active", "Failed to write active plan marker", true),
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
        assert_eq!(
            error.contains("Plan documents already saved: corrected"),
            saved
        );
        let document = plans.join("corrected.json");
        assert_eq!(document.is_file(), saved);
        if saved {
            let stored = PlanStorage::load_plan(root.path(), "corrected")
                .unwrap()
                .unwrap();
            assert_eq!(stored.title, "Corrected");
            assert!(plans.join("active").is_dir());
        }
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
