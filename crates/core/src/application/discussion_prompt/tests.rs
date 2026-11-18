use super::*;
use crate::plan::{storage::PlanStorage, Plan};
use crate::review::{storage::ReviewStorage, ReviewItem, ReviewSession, DEFAULT_REVIEW_GROUP_ID};

fn anchored(id: &str) -> DiscussionFocus {
    DiscussionFocus::Focused {
        target: DiscussionTarget::Plan { id: id.into() },
        focused_items: vec!["1".into()],
    }
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
    assert!(prepared.prompt.contains("Selected plan"));
    assert!(prepared.prompt.contains("Keep this comment"));
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
    assert_eq!(prepared.prompt, text);
    assert!(prepared.focus.is_none());
    assert!(prepared.focus_update.unwrap().changed);
    assert_eq!(app.discussion_focus().await, DiscussionFocus::None);
    let free = app
        .prepare_model_ask(root.path(), ModelAskInput::Prompt { text: text.into() })
        .await
        .unwrap();
    assert_eq!(free.prompt, text);
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
    assert_eq!(fallback.prompt, "Continue");
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
    assert!(prepared.prompt.contains("First member"));
    assert!(prepared.prompt.contains("Second member"));
    assert!(!prepared.prompt.contains("Foreign member"));
    assert!(prepared.prompt.contains("Group comment"));
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
