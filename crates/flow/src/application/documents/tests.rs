struct Observer;
impl ApplicationObserver for Observer {
    fn publish(&self, _id: &str, _event: ApplicationEvent) {}
}

use super::*;
use tauqe_core::config::AppConfig;
fn application(root: &Path) -> Application {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    Application::for_repository(AppConfig::default(), root.to_path_buf(), events)
}
fn plan(id: &str) -> Plan {
    serde_json::from_value(serde_json::json!({"id":id,"title":id,"created_at":1,"updated_at":1,"concept":[],"implementation":{"items":[{"id":"1","title":"Step","status":"todo","files":["./src//lib.rs"]}]}})).unwrap()
}
#[tokio::test]
async fn selection_is_read_only_and_never_repairs_a_missing_or_stale_active_marker() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let documents = app.documents_at(root.path());
    app.save_plan(plan("a"), &Observer).await.unwrap();
    app.save_plan(plan("b"), &Observer).await.unwrap();
    app.set_active_plan(None, &Observer).await.unwrap();
    assert!(documents.resolve_plan(None).await.unwrap().is_some());
    assert!(documents.active_plan_id().await.unwrap().is_none());
    app.set_active_plan(Some("missing".into()), &Observer)
        .await
        .unwrap();
    assert!(documents.resolve_plan(None).await.unwrap().is_none());
    assert_eq!(
        documents.active_plan_id().await.unwrap().as_deref(),
        Some("missing")
    );
    assert_eq!(
        documents
            .resolve_plan(Some("b".into()))
            .await
            .unwrap()
            .unwrap()
            .id,
        "b"
    );
    assert_eq!(
        documents.active_plan_id().await.unwrap().as_deref(),
        Some("missing")
    );
}
#[tokio::test]
async fn headless_save_enforces_identifiers_tree_integrity_normalization_and_admission() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let documents = app.documents_at(root.path());
    assert!(matches!(
        app.save_plan(plan("../escape"), &Observer).await,
        Err(DocumentError::InvalidPlanId)
    ));
    let mut invalid = plan("invalid");
    invalid
        .implementation
        .items
        .push(invalid.implementation.items[0].clone());
    assert!(matches!(
        app.save_plan(invalid, &Observer).await,
        Err(DocumentError::InvalidPlanTree(_))
    ));
    let mut candidate = plan("valid");
    candidate.implementation.items[0].readonly_files =
        vec!["src/lib.rs".into(), "reference.rs".into()];
    app.save_plan(candidate, &Observer).await.unwrap();
    let saved = app.get_plan(Some("valid".into())).await.unwrap().unwrap();
    assert_eq!(saved.implementation.items[0].files, ["./src//lib.rs"]);
    assert_eq!(
        saved.implementation.items[0].readonly_files,
        ["reference.rs"]
    );
    let operation = app.try_begin_operation().unwrap();
    assert!(matches!(
        app.save_plan(plan("other"), &Observer).await,
        Err(DocumentError::Busy)
    ));
    assert!(matches!(
        app.delete_plan("valid", &Observer).await,
        Err(DocumentError::Busy)
    ));
    assert!(documents.plan("valid").await.unwrap().is_some());
    drop(operation);
    assert!(app.delete_plan("valid", &Observer).await.unwrap());
}
#[tokio::test]
async fn review_update_requires_explicit_session_and_preserves_note_clear_semantics() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let session:ReviewSession=serde_json::from_value(serde_json::json!({"id":"review","created_at":1,"items":[{"id":1,"title":"Finding","note":"Original"}]})).unwrap();
    ReviewStorage::save_session(root.path(), &session).unwrap();
    assert!(matches!(
        app.update_review_item(None, 1, None, None, &Observer).await,
        Err(DocumentError::MissingReviewId)
    ));
    let item = app
        .update_review_item(
            Some(" review ".into()),
            1,
            Some(IssueStatus::Skipped),
            Some("  Decision  ".into()),
            &Observer,
        )
        .await
        .unwrap();
    let item = item.unwrap();
    assert_eq!(item.status, IssueStatus::Skipped);
    assert_eq!(item.note.as_deref(), Some("Decision"));
    let item = app
        .update_review_item(
            Some("review".into()),
            1,
            None,
            Some(" \n ".into()),
            &Observer,
        )
        .await
        .unwrap();
    assert!(item.unwrap().note.is_none());
    assert!(app
        .update_review_item(Some("missing".into()), 1, None, None, &Observer)
        .await
        .unwrap()
        .is_none());
}
#[tokio::test]
async fn deleting_a_document_releases_only_its_own_focus() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    app.save_plan(plan("a"), &Observer).await.unwrap();
    app.save_plan(plan("b"), &Observer).await.unwrap();
    let focus = tauqe_core::discussion::DiscussionFocus::Focused {
        target: DiscussionTarget::Plan { id: "a".into() },
        focused_items: vec![],
    };
    app.set_discussion_focus(focus.clone()).await;
    assert!(app.delete_plan("b", &Observer).await.unwrap());
    assert_eq!(app.discussion_focus().await, focus);
    assert!(app.delete_plan("a", &Observer).await.unwrap());
    assert_eq!(
        app.discussion_focus().await,
        tauqe_core::discussion::DiscussionFocus::None
    );
}
#[tokio::test]
async fn replan_checks_target_and_done_immutability_before_any_mutation() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let documents = app.documents_at(root.path());
    assert!(matches!(
        documents.prepare_plan_replan("".into(), "".into()).await,
        Err(PlanPreparationError::MissingId)
    ));
    app.save_plan(plan("a"), &Observer).await.unwrap();
    assert!(matches!(
        documents.prepare_plan_replan("a".into(), "".into()).await,
        Err(PlanPreparationError::MissingStep)
    ));
    assert!(matches!(
        documents
            .prepare_plan_replan("a".into(), "missing".into())
            .await,
        Err(PlanPreparationError::NotFound(_))
    ));
    assert_eq!(
        documents
            .prepare_plan_refinement("".into())
            .await
            .unwrap()
            .id,
        "a"
    );
    app.update_plan_item("a", "1", Some(PlanItemStatus::Done), &Observer)
        .await
        .unwrap();
    assert!(matches!(
        documents.prepare_plan_replan("a".into(), "1".into()).await,
        Err(PlanPreparationError::Immutable(_))
    ));
}
