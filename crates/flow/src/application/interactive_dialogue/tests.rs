use super::*;
use crate::api::Application;
use crate::application::infrastructure::execution::ExecutionFactory;
use std::sync::Arc;
use tauqe_core::config::AppConfig;
use tauqe_core::models::ProviderKind;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};

async fn execute(
    root: &Path,
    responses: Vec<MockResponse>,
    focus: Option<DiscussionTarget>,
    rounds: usize,
) -> (ReplArtifact, Vec<tauqe_core::providers::mock::MockRequest>) {
    let mut config: AppConfig =
        toml::from_str(&format!("[repl]\nmax_turn_rounds = {rounds}\n")).unwrap();
    config.context.include_repo_map = false;
    let app = Application::new(config);
    let provider = Arc::new(MockLlmProvider::scripted(responses));
    app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
        .await;
    let operation = app.try_begin_operation().unwrap();
    let factory = ExecutionFactory::bind(&app, &operation).await.unwrap();
    let mut dialogue = factory.for_dialogue().await.unwrap();
    let mut context = ContextManager::new(root.to_path_buf());
    let request = InteractiveExecutionRequest {
        capabilities: tauqe_core::control::repl_capabilities(focus.as_ref()),
        prompt: "Discuss the change".into(),
        focus,
    };
    let result = run(
        &mut dialogue,
        root,
        &mut context,
        &CancellationToken::new(),
        &EventSink::noop(),
        request,
        None,
    )
    .await
    .unwrap()
    .into_success()
    .unwrap();
    (result.value, provider.requests())
}

fn plan_payload(concept: bool) -> String {
    serde_json::json!({
        "message": "Plan response",
        "action": {"kind": "plan", "plan_create": {
            "action": "save", "id": "corrected", "title": "Corrected",
            "concept": if concept { vec![serde_json::json!({"id":"c-1","title":"Invariant"})] } else { vec![] },
            "items": [{"id":"1","title":"Step","files":["src/step.rs"]}]
        }, "commit_message":"create corrected plan", "reason":""}
    }).to_string()
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
        assert_eq!(result.plans_applied, expected_applied);
        assert_eq!(
            tauqe_core::plan::storage::PlanStorage::load_plan(root.path(), "corrected")
                .unwrap()
                .is_some(),
            expected_applied
        );
        if expected_applied {
            assert!(result.plan_rejections.is_empty());
            assert!(requests[1]
                .messages
                .iter()
                .any(|message| message.content.contains("[Plan Correction 1]")));
        } else {
            assert_eq!(
                result.plan_rejections[0].kind,
                PlanRejectionKind::PayloadDefect
            );
        }
    }
}

#[tokio::test]
async fn review_decisions_address_only_the_frozen_focus() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    for id in ["focused", "other"] {
        let session = tauqe_core::review::ReviewSession {
            id: id.into(),
            title: id.into(),
            items: vec![
                serde_json::from_value(serde_json::json!({"id":7,"title":"Race"})).unwrap(),
            ],
            ..Default::default()
        };
        ReviewStorage::save_session(root.path(), &session).unwrap();
    }
    let response = serde_json::json!({
        "message":"Decision recorded.",
        "action":{"kind":"review","review_update":{"item_id":7,"group_id":null,
            "status":"rejected","note":"Already verified","recommendations":[]},
            "commit_message":"","reason":""}
    });
    let (result, _) = execute(
        root.path(),
        vec![MockResponse::text(response.to_string())],
        Some(DiscussionTarget::Review {
            id: "focused".into(),
        }),
        2,
    )
    .await;
    assert_eq!(result.review_decisions_applied, [7]);
    assert_eq!(result.message.as_deref(), Some("Decision recorded."));
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
    assert!(created.plans_applied);
    let (refused, requests) = execute(
        root.path(),
        vec![MockResponse::text(plan_payload(true))],
        None,
        2,
    )
    .await;
    assert_eq!(requests.len(), 1);
    assert!(!refused.plans_applied);
    assert_eq!(
        refused.plan_rejections[0].kind,
        PlanRejectionKind::ModeForbidden
    );
}
