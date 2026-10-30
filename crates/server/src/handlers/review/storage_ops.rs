//! Review session CRUD RPC handlers.
//!
//! List, get, delete, and update-item operations share the storage-offload and
//! event-emission helpers from the sibling `common` module and never resolve a
//! session through the process-global active marker for mutations: every update
//! is scoped to the explicitly named `review_id`.

use std::sync::Arc;

use tauqe_core::review::ReviewStorage;
use tauqe_protocol::discussion::DiscussionTarget;
use tauqe_protocol::{
    errors, events, Request, Response, ReviewDeleteParams, ReviewDeleteResult, ReviewGetParams,
    ReviewGetResult, ReviewListChangedEvent, ReviewListResult, ReviewUpdateItemParams,
    ReviewUpdateItemResult, ReviewUpdatedEvent,
};

use crate::handlers::common::release_focus_anchored_on;
use crate::state::AppState;

use super::common::{emit, offload_review};

pub async fn handle_review_list(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let load_root = repo_root.clone();
    match offload_review(move || ReviewStorage::load_all(&load_root)).await {
        Ok(reviews) => {
            let active_root = repo_root.clone();
            let active_id = offload_review(move || ReviewStorage::load_active_id(&active_root))
                .await
                .unwrap_or(None);
            Response::ok_typed(req.id, &ReviewListResult { reviews, active_id })
        }
        Err(err) => Response::err(req.id, errors::REVIEW_LIST_FAILED, err),
    }
}

/// Resolves a review session for display: the explicitly named one, the active
/// one, or the first stored session.
///
/// Resolution is a pure read. It never persists the session it fell back to,
/// so reading never rewrites which session the client renders as active;
/// promoting a session is an explicit mutation.
pub async fn handle_review_get(req: Request, state: &Arc<AppState>) -> Response {
    // Strict wire-contract parse: a present payload must decode through the
    // typed DTO, so a malformed `id` answers INVALID_PARAMS instead of
    // silently resolving through the active session (Conventions §2.5). An
    // absent payload or an empty id still means "the active session".
    let target_id = match req.params {
        None => None,
        Some(payload) => match serde_json::from_value::<ReviewGetParams>(payload) {
            Ok(params) => params.id.filter(|id| !id.trim().is_empty()),
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid review get parameters: {}", err),
                );
            }
        },
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();

    let active_id = {
        let root = repo_root.clone();
        offload_review(move || ReviewStorage::load_active_id(&root))
            .await
            .unwrap_or(None)
    };
    // Resolution never persists the fallback it picked: promoting a session is
    // an explicit mutation, so a read RPC leaves the stored active pointer as
    // it found it.
    let review_id = match &target_id {
        Some(id) => Some(id.clone()),
        None => match active_id {
            Some(id) => Some(id),
            None => {
                let root = repo_root.clone();
                offload_review(move || {
                    ReviewStorage::list_reviews(&root)
                        .map(|reviews| reviews.into_iter().next().map(|r| r.id))
                })
                .await
                .unwrap_or(None)
            }
        },
    };

    match review_id {
        Some(id) => {
            let root = repo_root.clone();
            match offload_review(move || ReviewStorage::load_review(&root, &id)).await {
                Ok(session) => Response::ok_typed(req.id, &ReviewGetResult { session }),
                Err(err) => Response::err(req.id, errors::REVIEW_GET_FAILED, err),
            }
        }
        None => Response::ok_typed(req.id, &ReviewGetResult { session: None }),
    }
}

pub async fn handle_review_delete(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot delete a review while a model operation is in progress",
    ) {
        return resp;
    }

    let params: ReviewDeleteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid review ID",
            )
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let delete_root = repo_root.clone();
    let review_id = params.id.clone();
    match offload_review(move || ReviewStorage::delete_review(&delete_root, &review_id)).await {
        Ok(deleted) => {
            if deleted {
                // Discussion Focus invariant: an anchor may never address a
                // deleted entity. A focus bound to the removed session is
                // released together with it through the shared release helper,
                // which broadcasts the release so every client drops the dead
                // anchor instead of rendering it.
                release_focus_anchored_on(
                    state,
                    &DiscussionTarget::Review {
                        id: params.id.clone(),
                    },
                )
                .await;

                let list_root = repo_root.clone();
                if let Ok(reviews) =
                    offload_review(move || ReviewStorage::load_all(&list_root)).await
                {
                    let active_root = repo_root.clone();
                    let active_id =
                        offload_review(move || ReviewStorage::load_active_id(&active_root))
                            .await
                            .unwrap_or(None);
                    emit(
                        &state.out,
                        events::REVIEW_LIST_CHANGED,
                        serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
                    );
                }
            }
            Response::ok_typed(req.id, &ReviewDeleteResult { success: deleted })
        }
        Err(err) => Response::err(req.id, errors::REVIEW_DELETE_FAILED, err),
    }
}

pub async fn handle_review_update_item(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot update a review item while a model operation is in progress",
    ) {
        return resp;
    }

    let params: ReviewUpdateItemParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing or invalid review item update",
                );
            }
        };

    // Strict Review ID isolation: an update is scoped to the session the caller
    // names. The process-global active marker is a UI pointer only and is never
    // consulted as an implicit fallback, so a missing id is rejected instead of
    // silently mutating whichever session happens to be active.
    let review_id = match params
        .review_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(id) => id.to_string(),
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "review_id is required: review item updates are scoped to an explicit session",
            );
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let update_root = repo_root.clone();
    let update_review_id = review_id.clone();
    let item_id = params.item_id;
    let status = params.status;
    let note = params.note.clone();

    let result = offload_review(move || {
        ReviewStorage::update_session_item(&update_root, &update_review_id, item_id, |item| {
            if let Some(status) = status {
                item.status = status;
            }
            if let Some(notes) = note {
                let trimmed = notes.trim();
                item.note = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
        })
    })
    .await;

    match result {
        Ok(Some(item)) => {
            let load_root = repo_root.clone();
            let load_id = review_id;
            let session = offload_review(move || ReviewStorage::load_review(&load_root, &load_id))
                .await
                .ok()
                .flatten();

            if let Some(session) = session {
                emit(
                    &state.out,
                    events::REVIEW_UPDATED,
                    serde_json::to_value(ReviewUpdatedEvent { session }).ok(),
                );
            }
            let list_root = repo_root.clone();
            if let Ok(reviews) = offload_review(move || ReviewStorage::load_all(&list_root)).await {
                let active_root = repo_root.clone();
                let active_id = offload_review(move || ReviewStorage::load_active_id(&active_root))
                    .await
                    .unwrap_or(None);
                emit(
                    &state.out,
                    events::REVIEW_LIST_CHANGED,
                    serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
                );
            }
            Response::ok_typed(req.id, &ReviewUpdateItemResult { item })
        }
        Ok(None) => Response::err(
            req.id,
            errors::NOT_FOUND,
            format!("Review item #{} not found", params.item_id),
        ),
        Err(err) => Response::err(req.id, errors::REVIEW_UPDATE_FAILED, err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::{scratch_root, snapshot_tree};
    use crate::transport::OutChannel;
    use tauqe_core::config::AppConfig;
    use tauqe_protocol::{RequestId, ReviewSession};

    /// Builds the harness state a read-path regression test runs against.
    fn state_over(root: std::path::PathBuf) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(AppConfig::default(), root, history_sender, out)
    }

    /// Builds an id-less `review/get`, the request that used to promote an
    /// active session as a side effect.
    fn review_get_request() -> Request {
        Request {
            id: RequestId::Number(1),
            method: tauqe_protocol::methods::REVIEW_GET.to_string(),
            params: None,
        }
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

    /// Resolves a stored session while nothing is active, and persists nothing.
    #[tokio::test]
    async fn review_get_resolves_without_persisting_an_active_pointer() {
        let root = scratch_root("review-get-read-only");
        let session = stored_session("review-1", 1);
        ReviewStorage::save_session(&root, &session).expect("save the stored session");
        // Persisting a session promotes the first stored one so a client always
        // has a marker to render. The read path is what must never promote, so
        // the pointer is cleared first and has to stay cleared.
        ReviewStorage::save_active_id(&root, None).expect("clear the active pointer");
        // The scratch repository and the harness state are materialized before
        // the snapshot, so the comparison captures exactly what the request
        // itself changed.
        let state = state_over(root.clone());
        let before = snapshot_tree(&root);

        let response = handle_review_get(review_get_request(), &state).await;
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
        let active = ReviewStorage::load_active_id(&root).expect("read the active pointer");
        assert!(
            active.is_none(),
            "review/get must never promote an active session as a read side effect"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Resolves the active session and keeps the stored pointer untouched, even
    /// though another stored session is enumerated first.
    #[tokio::test]
    async fn review_get_preserves_an_existing_active_pointer() {
        let root = scratch_root("review-get-preserve");
        let first = stored_session("review-a", 1);
        ReviewStorage::save_session(&root, &first).expect("save the first session");
        let second = stored_session("review-b", 2);
        ReviewStorage::save_session(&root, &second).expect("save the second session");
        ReviewStorage::save_active_id(&root, Some("review-b")).expect("store the active id");
        let state = state_over(root.clone());
        let before = snapshot_tree(&root);

        let response = handle_review_get(review_get_request(), &state).await;
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
            Some("review-b"),
            "the active session wins over the first enumerated session"
        );
        assert_eq!(
            snapshot_tree(&root),
            before,
            "a read RPC must not rewrite the persisted active pointer"
        );
        let active = ReviewStorage::load_active_id(&root).expect("read the active pointer");
        assert_eq!(
            active,
            Some("review-b".to_string()),
            "the stored pointer still names the active session"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
