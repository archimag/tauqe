use super::*;
use crate::api::classify_interactive_completion;
use tauqe_core::control::outcome::SystemError;
fn completion() -> InteractiveTurnCompletion {
    classify_interactive_completion(Err(SystemError::Other("unused".into())), String::new())
}
#[tokio::test]
async fn plan_updates_preserve_active_selection_and_do_not_replace_a_stale_active_id() {
    let root = tempfile::tempdir().unwrap();
    let app = Application::default();
    let mut completion = completion();
    assert!(app
        .interactive_plan_updates(root.path(), &completion)
        .await
        .is_none());
    for id in ["first", "second"] {
        PlanStorage::save_plan(
            root.path(),
            &Plan {
                id: id.into(),
                title: id.into(),
                ..Default::default()
            },
        )
        .unwrap();
    }
    completion.plans_applied = true;
    PlanStorage::save_active_id(root.path(), Some("second")).unwrap();
    let updates = app
        .interactive_plan_updates(root.path(), &completion)
        .await
        .unwrap();
    assert_eq!(updates.active_id.as_deref(), Some("second"));
    assert_eq!(updates.active_plan.unwrap().id, "second");
    assert_eq!(updates.plans.len(), 2);
    std::fs::write(
        PlanStorage::plans_dir(root.path()).join("active"),
        "missing",
    )
    .unwrap();
    let updates = app
        .interactive_plan_updates(root.path(), &completion)
        .await
        .unwrap();
    assert_eq!(updates.active_id.as_deref(), Some("missing"));
    assert!(updates.active_plan.is_none());
    assert_eq!(updates.plans.len(), 2);
}
#[tokio::test]
async fn missing_or_non_directory_plan_paths_keep_empty_list_publication() {
    let root = tempfile::tempdir().unwrap();
    let app = Application::default();
    let mut completion = completion();
    completion.plans_applied = true;
    let updates = app
        .interactive_plan_updates(root.path(), &completion)
        .await
        .unwrap();
    assert!(updates.plans.is_empty());
    std::fs::create_dir_all(root.path().join(".tauqe")).unwrap();
    std::fs::write(PlanStorage::plans_dir(root.path()), "not a directory").unwrap();
    let updates = app
        .interactive_plan_updates(root.path(), &completion)
        .await
        .unwrap();
    assert!(updates.plans.is_empty());
    assert!(updates.active_plan.is_none());
}
#[tokio::test]
async fn review_updates_resolve_both_review_targets_and_preserve_decision_multiplicity() {
    let root = tempfile::tempdir().unwrap();
    let app = Application::default();
    let mut completion = completion();
    let review = ReviewSession {
        id: "review".into(),
        title: "Current content".into(),
        ..Default::default()
    };
    ReviewStorage::save_session(root.path(), &review).unwrap();
    let focus = DiscussionTarget::Review {
        id: "review".into(),
    };
    assert!(app
        .interactive_review_updates(root.path(), &completion, Some(&focus))
        .await
        .is_none());
    completion.review_decisions_applied = vec![7, 7, 9];
    for target in [
        focus,
        DiscussionTarget::ReviewGroup {
            session_id: "review".into(),
            group_id: "group".into(),
        },
    ] {
        let updates = app
            .interactive_review_updates(root.path(), &completion, Some(&target))
            .await
            .unwrap();
        assert_eq!(updates.session, review);
        assert_eq!(updates.decisions_applied, 3);
    }
    for target in [
        None,
        Some(DiscussionTarget::Plan {
            id: "review".into(),
        }),
        Some(DiscussionTarget::Review {
            id: "missing".into(),
        }),
    ] {
        assert!(app
            .interactive_review_updates(root.path(), &completion, target.as_ref())
            .await
            .is_none());
    }
}
