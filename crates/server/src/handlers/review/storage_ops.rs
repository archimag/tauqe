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
    let active_missing = active_id.is_none();

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

    if target_id.is_none() && active_missing {
        if let Some(id) = review_id.clone() {
            let root = repo_root.clone();
            let _ = offload_review(move || ReviewStorage::save_active_id(&root, Some(id.as_str())))
                .await;
        }
    }

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
