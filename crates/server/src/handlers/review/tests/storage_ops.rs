//! `review/get`, `review/updateItem`, and `review/delete` contracts.
//!
//! Resolution is a pure read that never promotes the session it fell back to,
//! a mutation is scoped to the explicitly named session, and deleting a session
//! releases a discussion focus anchored on it.

use std::sync::Arc;

use super::super::{handle_review_delete, handle_review_get, handle_review_update_item};

use crate::handlers::common::test_support::{scratch_root, snapshot_tree};
use crate::state::AppState;
use crate::transport::OutChannel;

use tauqe_core::config::AppConfig;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::discussion::DiscussionFocus;
use tauqe_protocol::{errors, events, Request, RequestId, ReviewGetResult, ReviewSession};

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

    // The release is broadcast as a typed payload every client reconstructs its
    // anchor from.
    let mut release = None;
    while let Ok(raw) = rx.try_recv() {
        if raw.contains(events::DISCUSSION_FOCUS_CHANGED) {
            release = Some(raw);
        }
    }
    let release = release.expect("the focus release must be broadcast");
    assert!(
        release.contains("\"kind\":\"none\""),
        "the release must carry an explicit detached focus: {release}"
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
