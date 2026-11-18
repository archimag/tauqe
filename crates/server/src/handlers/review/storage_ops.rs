//! Review document RPC adaptation.
use super::common::{publish_review, publish_review_list};
use crate::contracts::Project;
use crate::state::AppState;
use std::sync::Arc;
#[cfg(test)]
#[cfg(test)]
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    errors, Request, Response, ReviewDeleteParams, ReviewDeleteResult, ReviewGetParams,
    ReviewGetResult, ReviewListResult, ReviewUpdateItemParams, ReviewUpdateItemResult,
};
pub async fn handle_review_list(req: Request, state: &Arc<AppState>) -> Response {
    let root = state.repo_root().await;
    let documents = state.application.documents_at(&root);
    match documents.reviews().await {
        Ok(reviews) => {
            let active_id = documents.active_review_id().await.unwrap_or(None);
            Response::ok_typed(
                req.id,
                &ReviewListResult {
                    reviews: reviews.into_iter().map(Project::project).collect(),
                    active_id,
                },
            )
        }
        Err(error) => Response::err(req.id, errors::REVIEW_LIST_FAILED, error),
    }
}
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

    let root = state.repo_root().await;
    match state
        .application
        .documents_at(&root)
        .resolve_review(target_id)
        .await
    {
        Ok(session) => Response::ok_typed(
            req.id,
            &ReviewGetResult {
                session: session.map(Project::project),
            },
        ),
        Err(error) => Response::err(req.id, errors::REVIEW_GET_FAILED, error),
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

    let root = state.repo_root().await;
    match state
        .application
        .documents_at(&root)
        .delete_review(&params.id)
        .await
    {
        Ok(outcome) => {
            if let Some(update) = outcome.focus_update {
                state.publish_focus_update(update);
            }
            if outcome.deleted {
                publish_review_list(state, &root).await;
            }
            Response::ok_typed(
                req.id,
                &ReviewDeleteResult {
                    success: outcome.deleted,
                },
            )
        }
        Err(error) => {
            let code = if matches!(&error, tauqe_core::api::DocumentError::Busy) {
                errors::OPERATION_IN_PROGRESS
            } else {
                errors::REVIEW_DELETE_FAILED
            };
            Response::err(req.id, code, error.to_string())
        }
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

    let root = state.repo_root().await;
    match state
        .application
        .documents_at(&root)
        .update_review_item(
            params.review_id,
            params.item_id,
            params.status.map(Project::project),
            params.note,
        )
        .await
    {
        Ok((review_id, Some(item))) => {
            publish_review(state, &root, &review_id).await;
            Response::ok_typed(
                req.id,
                &ReviewUpdateItemResult {
                    item: item.project(),
                },
            )
        }
        Ok((_, None)) => Response::err(
            req.id,
            errors::NOT_FOUND,
            format!("Review item #{} not found", params.item_id),
        ),
        Err(error) => {
            let code = match &error {
                tauqe_core::api::DocumentError::MissingReviewId => errors::INVALID_PARAMS,
                tauqe_core::api::DocumentError::Busy => errors::OPERATION_IN_PROGRESS,
                _ => errors::REVIEW_UPDATE_FAILED,
            };
            Response::err(req.id, code, error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::{scratch_root, snapshot_tree};
    use crate::transport::OutChannel;
    use tauqe_core::config::AppConfig;
    use tauqe_core::review::ReviewSession;
    use tauqe_protocol::RequestId;

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
