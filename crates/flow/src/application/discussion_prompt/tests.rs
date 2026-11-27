use super::*;
use tauqe_core::plan::{storage::PlanStorage, Plan};
use tauqe_core::review::{
    storage::ReviewStorage, ReviewItem, ReviewSession, DEFAULT_REVIEW_GROUP_ID,
};

fn anchored(id: &str) -> DiscussionFocus {
    DiscussionFocus::Focused {
        target: DiscussionTarget::Plan { id: id.into() },
        focused_items: vec!["1".into()],
    }
}

#[tokio::test]
async fn plan_focus_renders_the_current_persisted_blocker_on_each_request() {
    use tauqe_core::plan::{PlanItem, PlanItemStatus};

    let root = tempfile::tempdir().unwrap();
    let app = Application::default();
    let mut plan = Plan {
        id: "blocked-plan".into(),
        title: "Blocked plan".into(),
        ..Default::default()
    };
    plan.implementation.items = vec![PlanItem {
        id: "1".into(),
        title: "Implement contract".into(),
        status: PlanItemStatus::Blocked,
        blocked_reason: Some("Missing <contract>".into()),
        files: vec!["src/lib.rs".into()],
        ..Default::default()
    }];
    PlanStorage::save_plan(root.path(), &plan).unwrap();
    let explicit = app
        .prepare_model_ask(
            root.path(),
            ModelAskInput::Discussion {
                target: DiscussionTarget::Plan {
                    id: plan.id.clone(),
                },
                focused_items: vec!["1".into()],
                comment: "Why is this blocked?".into(),
            },
        )
        .await
        .unwrap();
    let DiscussionDocument::Plan {
        plan: loaded_plan,
        focused_items,
    } = explicit.discussion.unwrap().document
    else {
        panic!("expected a resolved plan");
    };
    let prompt = tauqe_core::plan::prompt::format_plan_discussion_prompt(
        &loaded_plan,
        &focused_items,
        &explicit.utterance,
    );
    assert!(prompt.contains("<blocked_reason>Missing &lt;contract&gt;</blocked_reason>"));

    plan.implementation.items[0].blocked_reason = Some("Dependency is unavailable".into());
    PlanStorage::save_plan(root.path(), &plan).unwrap();
    let implicit = app
        .prepare_model_ask(
            root.path(),
            ModelAskInput::Prompt {
                text: "Explain the current blocker".into(),
            },
        )
        .await
        .unwrap();
    let DiscussionDocument::Plan {
        plan: loaded_plan,
        focused_items,
    } = implicit.discussion.unwrap().document
    else {
        panic!("expected a resolved plan");
    };
    let prompt = tauqe_core::plan::prompt::format_plan_discussion_prompt(
        &loaded_plan,
        &focused_items,
        &implicit.utterance,
    );
    assert!(prompt.contains("<blocked_reason>Dependency is unavailable</blocked_reason>"));
    assert!(!prompt.contains("Missing &lt;contract&gt;"));
    assert_eq!(app.discussion_focus().await, anchored("blocked-plan"));
}

#[tokio::test]
async fn explicit_discussion_loads_before_replacing_focus_and_preserves_repeat_notifications() {
    let root = tempfile::tempdir().unwrap();
    let app = Application::default();
    app.set_discussion_focus(anchored("previous")).await;
    let request = || ModelAskInput::Discussion {
        target: DiscussionTarget::Plan { id: "plan".into() },
        focused_items: vec!["1".into(), "1".into()],
        comment: "Keep this comment".into(),
    };
    let error = app
        .prepare_model_ask(root.path(), request())
        .await
        .unwrap_err();
    assert!(matches!(error, DiscussionPromptError::NotFound(_)));
    assert_eq!(app.discussion_focus().await, anchored("previous"));
    PlanStorage::save_plan(
        root.path(),
        &Plan {
            id: "plan".into(),
            title: "Selected plan".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let prepared = app.prepare_model_ask(root.path(), request()).await.unwrap();
    assert_eq!(prepared.utterance, "Keep this comment");
    let discussion = prepared.discussion.unwrap();
    assert_eq!(
        discussion.target,
        DiscussionTarget::Plan { id: "plan".into() }
    );
    assert!(
        matches!(discussion.document, DiscussionDocument::Plan { plan, focused_items }
        if plan.title == "Selected plan" && focused_items == ["1", "1"])
    );
    let update = prepared.focus_update.unwrap();
    assert!(update.changed);
    assert!(
        matches!(update.focus,DiscussionFocus::Focused {focused_items,..} if focused_items==["1","1"])
    );
    assert!(
        !app.prepare_model_ask(root.path(), request())
            .await
            .unwrap()
            .focus_update
            .unwrap()
            .changed
    );
}

#[tokio::test]
async fn invalid_text_preserves_focus_and_stale_focus_falls_back_to_exact_text() {
    let root = tempfile::tempdir().unwrap();
    let app = Application::default();
    app.set_discussion_focus(anchored("missing")).await;
    assert!(matches!(
        app.prepare_model_ask(
            root.path(),
            ModelAskInput::Prompt {
                text: " \n ".into()
            }
        )
        .await,
        Err(DiscussionPromptError::InvalidInput)
    ));
    assert_eq!(app.discussion_focus().await, anchored("missing"));
    let text = "  Original text\n";
    let prepared = app
        .prepare_model_ask(root.path(), ModelAskInput::Prompt { text: text.into() })
        .await
        .unwrap();
    assert_eq!(prepared.utterance, text);
    assert!(prepared.discussion.is_none());
    assert!(prepared.focus_update.unwrap().changed);
    assert_eq!(app.discussion_focus().await, DiscussionFocus::None);
    let free = app
        .prepare_model_ask(root.path(), ModelAskInput::Prompt { text: text.into() })
        .await
        .unwrap();
    assert_eq!(free.utterance, text);
    assert!(free.discussion.is_none());
    assert!(free.focus_update.is_none());
}

#[tokio::test]
async fn corrupt_target_is_refused_explicitly_and_dropped_when_implicitly_focused() {
    let root = tempfile::tempdir().unwrap();
    PlanStorage::save_plan(
        root.path(),
        &Plan {
            id: "broken".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let plan_file = PlanStorage::plans_dir(root.path()).join("broken.json");
    std::fs::write(plan_file, "invalid document").unwrap();
    let app = Application::default();
    app.set_discussion_focus(anchored("broken")).await;
    let error = app
        .prepare_model_ask(
            root.path(),
            ModelAskInput::Discussion {
                target: DiscussionTarget::Plan {
                    id: "broken".into(),
                },
                focused_items: vec![],
                comment: String::new(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, DiscussionPromptError::Storage(_)));
    assert_eq!(app.discussion_focus().await, anchored("broken"));
    let fallback = app
        .prepare_model_ask(
            root.path(),
            ModelAskInput::Prompt {
                text: "Continue".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(fallback.utterance, "Continue");
    assert!(fallback.discussion.is_none());
    assert!(fallback.focus_update.unwrap().changed);
}

#[tokio::test]
async fn group_discussion_uses_effective_membership_instead_of_focused_item_selection() {
    let root = tempfile::tempdir().unwrap();
    let finding = |id, title, group| {
        serde_json::from_value::<ReviewItem>(
            serde_json::json!({"id":id,"title":title,"group":group}),
        )
        .unwrap()
    };
    let session = ReviewSession {
        id: "review".into(),
        title: "Review".into(),
        items: vec![
            finding(1, "First member", None::<&str>),
            finding(2, "Second member", Some(DEFAULT_REVIEW_GROUP_ID)),
            finding(3, "Foreign member", Some("other")),
        ],
        ..Default::default()
    };
    ReviewStorage::save_session(root.path(), &session).unwrap();
    let app = Application::default();
    let prepared = app
        .prepare_model_ask(
            root.path(),
            ModelAskInput::Discussion {
                target: DiscussionTarget::ReviewGroup {
                    session_id: "review".into(),
                    group_id: DEFAULT_REVIEW_GROUP_ID.into(),
                },
                focused_items: vec!["1".into()],
                comment: "Group comment".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(prepared.utterance, "Group comment");
    let DiscussionDocument::ReviewGroup { findings, .. } = prepared.discussion.unwrap().document
    else {
        panic!("expected a resolved review group");
    };
    assert_eq!(
        findings.iter().map(|item| item.id).collect::<Vec<_>>(),
        [1, 2]
    );
    let previous = app.discussion_focus().await;
    let error = app
        .prepare_model_ask(
            root.path(),
            ModelAskInput::Discussion {
                target: DiscussionTarget::ReviewGroup {
                    session_id: "review".into(),
                    group_id: "missing".into(),
                },
                focused_items: vec![],
                comment: String::new(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, DiscussionPromptError::NotFound(_)));
    assert_eq!(app.discussion_focus().await, previous);
}
