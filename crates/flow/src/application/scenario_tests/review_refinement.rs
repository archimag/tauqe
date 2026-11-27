use super::*;
use crate::workflows::review::refinement::{test_gates, GroupRefineError, GroupRefineInput};
use tauqe_core::plan::PlanImplementation;
use tauqe_core::review::{
    storage::ReviewStorage, IssueStatus, ReviewGroup, ReviewGroupKind, ReviewItem, ReviewSession,
};

fn saved(root: &Path) -> Vec<u8> {
    let session = ReviewSession {
        id: "audit".into(),
        title: "Audit".into(),
        groups: vec![
            ReviewGroup {
                id: "arch".into(),
                kind: ReviewGroupKind::Architectural,
                blocked_reason: Some("old blocker".into()),
                implementation: PlanImplementation {
                    items: vec![
                        PlanItem {
                            id: "done".into(),
                            status: PlanItemStatus::Done,
                            commit: Some("old-commit".into()),
                            ..Default::default()
                        },
                        PlanItem {
                            id: "blocked".into(),
                            status: PlanItemStatus::Blocked,
                            blocked_reason: Some("old cause".into()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
            ReviewGroup {
                id: "neighbor".into(),
                ..Default::default()
            },
        ],
        items: vec![
            serde_json::from_value::<ReviewItem>(
                serde_json::json!({"id":1,"title":"Open","group":"arch","status":"todo"}),
            )
            .unwrap(),
            serde_json::from_value::<ReviewItem>(
                serde_json::json!({"id":2,"title":"Closed","group":"arch","status":"fixed"}),
            )
            .unwrap(),
        ],
        ..Default::default()
    };
    ReviewStorage::save_session(root, &session).unwrap();
    let path = root.join(".tauqe/reviews/audit.json");
    std::fs::read(path).unwrap()
}

fn input() -> GroupRefineInput {
    GroupRefineInput {
        review_id: "audit".into(),
        group_id: "arch".into(),
        planner_model: None,
        critic_model: None,
        skip_critique: true,
        instructions: None,
    }
}

fn response() -> MockResponse {
    metered(
        serde_json::json!({
            "status":"success", "explanation":"Repaired", "reason":null,
            "abort":{"is_aborted":false,"reason":"","recommendation":""},
            "context_requests":[],"repo_searches":[],"repo_maps":[],
            "items":[{"id":"replacement","title":"Ready","status":"todo","files":["source.rs"]}]
        })
        .to_string(),
    )
}

async fn reached(gate: &test_gates::Gate) {
    tokio::time::timeout(DEADLINE, gate.entered.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn cancellation_before_write_keeps_the_saved_session_byte_for_byte() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.rs"), "source").unwrap();
    let before = saved(root.path());
    let gates = Arc::new(test_gates::Gates::default());
    let _installed = test_gates::install(root.path(), gates.clone());
    let app = application(root.path());
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([response()])))
        .await;
    let (events, _) = mpsc::unbounded_channel();
    let running = app.refine_review_group(input(), Arc::new(Observer(events)), DEADLINE);
    tokio::pin!(running);
    tokio::select! { _ = reached(&gates.before_write) => {}, result = &mut running => panic!("early completion: {:?}", result.err()) }
    assert!(app.cancel_active_operation().is_some());
    gates.before_write.release.add_permits(1);
    assert!(matches!(running.await, Err(GroupRefineError::Cancelled)));
    assert_eq!(
        std::fs::read(root.path().join(".tauqe/reviews/audit.json")).unwrap(),
        before
    );
    released(&app).await;
}

#[tokio::test]
async fn admitted_write_survives_cancel_and_lost_waiter_until_publication() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    std::fs::write(root.path().join("source.rs"), "source").unwrap();
    let before = saved(root.path());
    let gates = Arc::new(test_gates::Gates::default());
    let _installed = test_gates::install(root.path(), gates.clone());
    gates.before_write.release.add_permits(1);
    let app = application(root.path());
    app.test_set_provider(Arc::new(MockLlmProvider::scripted([response()])))
        .await;
    let (events, mut received) = mpsc::unbounded_channel();
    let running = tokio::spawn({
        let app = app.clone();
        async move {
            app.refine_review_group(input(), Arc::new(Observer(events)), DEADLINE)
                .await
        }
    });
    reached(&gates.in_write).await;
    running.abort();
    assert!(matches!(running.await, Err(error) if error.is_cancelled()));
    assert!(app.cancel_active_operation().is_some());
    assert!(app.try_begin_operation().is_err());
    assert_eq!(
        std::fs::read(root.path().join(".tauqe/reviews/audit.json")).unwrap(),
        before
    );
    gates.in_write.release.add_permits(1);
    reached(&gates.before_terminal).await;
    assert!(app.try_begin_operation().is_err());
    let stored = ReviewStorage::load_review(root.path(), "audit")
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.groups[0].implementation.items[0].commit.as_deref(),
        Some("old-commit")
    );
    assert_eq!(stored.groups[0].implementation.items[1].id, "replacement");
    assert_eq!(stored.groups[0].blocked_reason, None);
    assert_eq!(stored.items[0].status, IssueStatus::Todo);
    assert_eq!(stored.items[1].status, IssueStatus::Fixed);
    assert!(received.try_recv().is_ok());
    gates.before_terminal.release.add_permits(1);
    released(&app).await;
    let mut terminal = 0;
    while let Ok((_, event)) = received.try_recv() {
        if let ApplicationEvent::Finished(_) = event {
            terminal += 1;
        }
    }
    assert_eq!(terminal, 1);
}
