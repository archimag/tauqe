//! Primary audit synthesis and cancellation handlers.
//!
//! Core filters and materializes the reviewer model's Structured Output, so
//! this adapter publishes only the decoded `summary` while raw JSON stays
//! inside the engine. Cancellation supports both a soft
//! stop (drain the current turn) and an immediate hard stop.

use crate::contracts::Project;
use std::sync::Arc;

use crate::reap::task_exit_deadline;
use crate::state::AppState;
use tauqe_flow::api::{
    PresetReviewPreparationError, ReviewStartError, ReviewStartInput,
    ReviewSynthesisPreparationError,
};
use tauqe_protocol::{errors, Request, Response, ReviewCancelParams, ReviewStartParams};

pub async fn handle_review_start(mut req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewStartParams = match req.params.take() {
        Some(p) => match serde_json::from_value(p) {
            Ok(p) => p,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid review parameters: {}", err),
                );
            }
        },
        None => ReviewStartParams::default(),
    };

    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    let input = ReviewStartInput {
        preset: params.preset,
        model: params.model.map(Project::project),
        path_patterns: params.path_patterns,
        prompt: params.user_prompt,
    };
    match state
        .application
        .start_review(input, observer, task_exit_deadline())
        .await
    {
        Ok(id) => Response::ok(req.id, serde_json::json!({"operation_id":id})),
        Err(error) => {
            let code = match &error {
                ReviewStartError::Busy => errors::OPERATION_IN_PROGRESS,
                ReviewStartError::Provider(_) => errors::NO_API_KEY,
                ReviewStartError::Window(ReviewSynthesisPreparationError::EmptyContext) => {
                    errors::EMPTY_CONTEXT
                }
                ReviewStartError::Preset(PresetReviewPreparationError::NotFound(_)) => {
                    errors::NOT_FOUND
                }
                _ => errors::INVALID_PARAMS,
            };
            Response::err(req.id, code, error.to_string())
        }
    }
}

pub async fn handle_review_cancel(req: Request, state: &Arc<AppState>) -> Response {
    // A present-but-malformed payload is refused instead of defaulted: silently
    // degrading to the default would answer a mistyped soft stop with the most
    // destructive cancellation, hard-cancelling a review the client asked to
    // drain. Only an absent payload keeps the default immediate abort that a
    // client predating two-mode cancellation sends.
    let params: ReviewCancelParams = match req.params {
        Some(params) => match serde_json::from_value(params) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid cancel parameters: {}", err),
                );
            }
        },
        None => ReviewCancelParams::default(),
    };

    crate::handlers::model::cancel(req.id, state, params.mode)
}
