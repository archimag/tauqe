//! `review/get`, `review/updateItem`, and `review/delete` contracts.
//!
//! Resolution is a pure read that never promotes the session it fell back to,
//! a mutation is scoped to the explicitly named session, and deleting a session
//! releases a discussion focus anchored on it.

use std::sync::Arc;

use super::super::{
    handle_review_delete, handle_review_get, handle_review_update_group_step,
    handle_review_update_item,
};

use crate::handlers::common::test_support::{scratch_root, snapshot_tree};
use crate::state::AppState;
use crate::transport::OutChannel;

use tauqe_core::config::AppConfig;
use tauqe_core::plan::{PlanImplementation, PlanItem, PlanItemStatus};
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::review::IssueStatus;
use tauqe_core::review::ReviewSession;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::discussion::DiscussionFocus;
use tauqe_protocol::{errors, events, Request, RequestId, ReviewGetResult};

/// Builds the harness state a read-path regression test runs against.
fn state_over(root: std::path::PathBuf) -> Arc<AppState> {
    let (out, _rx) = OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    AppState::new(AppConfig::default(), root, history_sender, out)
}

/// Builds a stored review session with a fixed identity.
fn stored_session(id: &str, created_at: u64) -> ReviewSession {
    ReviewSession {
        id: id.to_string(),
        title: id.to_string(),
        created_at,
        ..Default::default()
    }
}

/// Builds an id-less `review/get`, the request that used to promote an active
/// session as a side effect.
fn review_get_request(params: Option<serde_json::Value>) -> Request {
    Request {
        id: RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_GET.to_string(),
        params,
    }
}

#[tokio::test]
async fn review_get_resolves_a_session_without_promoting_it() {
    let root = scratch_root("review-get-read-only");
    ReviewStorage::save_session(&root, &stored_session("review-1", 1))
        .expect("save the stored session");
    // Persisting a session promotes the first stored one so a client always has
    // a marker to render; the read path is what must never promote.
    ReviewStorage::save_active_id(&root, None).expect("clear the active pointer");
    let state = state_over(root.clone());
    let before = snapshot_tree(&root);

    let response = handle_review_get(review_get_request(None), &state).await;
    assert!(
        response.error.is_none(),
        "review/get failed: {:?}",
        response.error
    );
    let result: ReviewGetResult =
        serde_json::from_value(response.result.expect("review/get result"))
            .expect("review/get result deserializes");
    assert_eq!(
        result.session.as_ref().map(|session| session.id.as_str()),
        Some("review-1"),
        "a stored session is still resolved without an active pointer"
    );
    assert_eq!(
        snapshot_tree(&root),
        before,
        "a read RPC must leave every persisted file byte-identical"
    );
    assert!(
        ReviewStorage::load_active_id(&root)
            .expect("read the active pointer")
            .is_none(),
        "review/get must never promote an active session as a read side effect"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn review_get_rejects_a_malformed_payload() {
    let root = scratch_root("review-get-malformed");
    let state = state_over(root.clone());

    let response = handle_review_get(
        review_get_request(Some(serde_json::json!({ "id": 123 }))),
        &state,
    )
    .await;

    // A present payload must decode through the typed DTO instead of silently
    // resolving through the active session.
    assert_eq!(
        response.error.expect("the request is refused").code,
        errors::INVALID_PARAMS
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn update_item_requires_an_explicit_review_identifier() {
    let root = scratch_root("review-update-explicit-id");
    let state = state_over(root.clone());

    let response = handle_review_update_item(
        Request {
            id: RequestId::Number(1),
            method: tauqe_protocol::methods::REVIEW_UPDATE_ITEM.to_string(),
            params: Some(serde_json::json!({ "item_id": 1 })),
        },
        &state,
    )
    .await;

    assert_eq!(
        response.error.expect("the request is refused").code,
        errors::INVALID_PARAMS
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn review_delete_releases_the_focus_anchored_on_the_deleted_session() {
    let root = scratch_root("review-delete-focus");
    for id in ["review-del", "review-keep"] {
        ReviewStorage::save_session(&root, &stored_session(id, 1)).expect("save a stored session");
    }
    let (out, mut rx) = OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

    state
        .set_discussion_focus(DiscussionFocus::review("review-del", Vec::new()))
        .await;
    // Setting the focus itself broadcasts a change; only the release that
    // accompanies the deletion is under test here.
    while rx.try_recv().is_ok() {}

    let delete_request = |id: u64, review_id: &str| Request {
        id: RequestId::Number(id),
        method: tauqe_protocol::methods::REVIEW_DELETE.to_string(),
        params: Some(serde_json::json!({ "id": review_id })),
    };

    let response = handle_review_delete(delete_request(1, "review-del"), &state).await;
    assert!(
        response.error.is_none(),
        "delete failed: {:?}",
        response.error
    );
    assert!(
        state.discussion_focus().await.is_none(),
        "a focus anchored on a deleted session must be dropped"
    );

    assert_eq!(response.result, Some(serde_json::json!({"success": true})));
    assert!(ReviewStorage::load_review(&root, "review-del")
        .unwrap()
        .is_none());
    let published: Vec<tauqe_protocol::Event> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|raw| serde_json::from_str(&raw).unwrap())
        .collect();
    assert_eq!(
        published
            .iter()
            .map(|event| event.method.as_str())
            .collect::<Vec<_>>(),
        [
            events::DISCUSSION_FOCUS_CHANGED,
            events::REVIEW_LIST_CHANGED
        ]
    );
    assert_eq!(
        published[0].params.as_ref().unwrap()["focus"]["kind"],
        "none"
    );

    // A focus anchored on a surviving session is untouched by another deletion.
    state
        .set_discussion_focus(DiscussionFocus::review("review-keep", Vec::new()))
        .await;
    let response = handle_review_delete(delete_request(2, "review-del"), &state).await;
    assert!(
        response.error.is_none(),
        "delete failed: {:?}",
        response.error
    );
    assert_eq!(
        state.discussion_focus().await,
        DiscussionFocus::review("review-keep", Vec::new())
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn manual_rpc_resumes_saved_group_for_explicit_and_session_execution() {
    use super::execute::{execute_and_collect, execute_request};

    for kind in ["group", "session"] {
        let root = scratch_root("review-manual-resume");
        tauqe_core::git::init_repository(Some(&root), true).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/target.rs"), "pub fn target() {}\n").unwrap();
        tauqe_core::git::cmd::stage_files(&root, &["src/target.rs".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(&root, "initial", false).unwrap();
        let mut session = super::architectural_session("rev-resume", "arch-1");
        let group = &mut session.groups[0];
        group.blocked_reason = Some("saved group diagnosis".into());
        group.resolution_notes = Some("saved decision".into());
        group.implementation = PlanImplementation {
            items: vec![
                PlanItem {
                    id: "done".into(),
                    status: PlanItemStatus::Done,
                    commit: Some("existing-receipt".into()),
                    ..Default::default()
                },
                PlanItem {
                    id: "blocked".into(),
                    status: PlanItemStatus::Blocked,
                    blocked_reason: Some("external condition".into()),
                    files: vec!["src/target.rs".into()],
                    ..Default::default()
                },
                PlanItem {
                    id: "next".into(),
                    status: PlanItemStatus::Todo,
                    depends_on: vec!["blocked".into()],
                    files: vec!["src/target.rs".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        ReviewStorage::save_session(&root, &session).unwrap();
        let mut config = super::horizon_config();
        config.context.include_repo_map = false;
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(edit_response(
                "pub fn target() {}",
                "pub fn target() { let a = 1; }",
            )),
            MockResponse::text(edit_response(
                "pub fn target() { let a = 1; }",
                "pub fn target() { let a = 2; }",
            )),
        ]));
        let (out, mut rx) = OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(config, root.clone(), history, out);
        state.application.test_set_provider(provider.clone()).await;
        let target = if kind == "group" {
            serde_json::json!({"kind": "group", "review_id": "rev-resume", "group_id": "arch-1"})
        } else {
            serde_json::json!({"kind": "session", "review_id": "rev-resume"})
        };
        if kind == "group" {
            let refused =
                super::super::handle_review_execute(execute_request(target.clone()), &state).await;
            assert_eq!(refused.error.unwrap().code, errors::GROUP_NEEDS_REFINEMENT);
        }
        assert!(provider.requests().is_empty());

        let response = handle_review_update_group_step(Request {
            id: RequestId::Number(2),
            method: tauqe_protocol::methods::REVIEW_UPDATE_GROUP_STEP.into(),
            params: Some(serde_json::json!({"review_id":"rev-resume", "group_id":"arch-1", "step_id":"blocked", "status":"todo"})),
        }, &state).await;
        assert!(response.error.is_none(), "{response:?}");
        let saved = ReviewStorage::load_review(&root, "rev-resume")
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.groups[0].implementation.items[0],
            session.groups[0].implementation.items[0]
        );
        assert_eq!(
            saved.groups[0].implementation.items[2],
            session.groups[0].implementation.items[2]
        );
        assert_eq!(
            saved.groups[0].implementation.items[1].status,
            PlanItemStatus::Todo
        );
        assert!(saved.groups[0].implementation.items[1]
            .blocked_reason
            .is_none());
        assert_eq!(
            saved.groups[0].blocked_reason,
            session.groups[0].blocked_reason
        );
        assert_eq!(
            saved.groups[0].resolution_notes,
            session.groups[0].resolution_notes
        );
        assert_eq!(saved.items, session.items);
        assert!(provider.requests().is_empty());
        let read = handle_review_get(
            review_get_request(Some(serde_json::json!({"id":"rev-resume"}))),
            &state,
        )
        .await;
        let result: ReviewGetResult = serde_json::from_value(read.result.unwrap()).unwrap();
        assert_eq!(
            result.session.unwrap().groups[0].implementation.items[1].status,
            tauqe_protocol::PlanItemStatus::Todo
        );
        while rx.try_recv().is_ok() {}

        let events = execute_and_collect(&state, &mut rx, execute_request(target)).await;
        assert_eq!(
            provider
                .requests()
                .iter()
                .map(|request| request.model.as_str())
                .collect::<Vec<_>>(),
            ["review/coder", "review/coder"]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.method == events::REVIEW_BATCH_FINISHED)
                .count(),
            1
        );
        let finished = ReviewStorage::load_review(&root, "rev-resume")
            .unwrap()
            .unwrap();
        assert_eq!(
            finished.groups[0].implementation.items[0].commit.as_deref(),
            Some("existing-receipt")
        );
        assert_eq!(
            finished.groups[0]
                .implementation
                .items
                .iter()
                .map(|step| step.status)
                .collect::<Vec<_>>(),
            vec![PlanItemStatus::Done; 3]
        );
        assert_eq!(finished.items[0].status, IssueStatus::Fixed);
        let _ = std::fs::remove_dir_all(root);
    }
}

fn edit_response(old: &str, new: &str) -> String {
    serde_json::json!({
        "message": "Applied change", "scratchpad": "",
        "action": {"kind": "edit", "patch": format!("*** Begin Patch\n*** Update File: src/target.rs\n@@\n-{old}\n+{new}\n*** End Patch"),
                   "commit_message": "fix(review): resume group horizon", "error_message": "", "reason": ""}
    }).to_string()
}
