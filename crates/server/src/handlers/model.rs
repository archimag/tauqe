//! Model turn subsystem facade.
//!
//! Projects model commands and application events onto the wire.

use crate::publication;

#[cfg(test)]
mod tests;

use crate::contracts::Project;
use std::sync::Arc;

use tauqe_protocol::model::{CancelMode, ModelCancelParams};
use tauqe_protocol::{
    DiscussionClearFocusResult, DiscussionGetFocusResult, DiscussionSetFocusParams,
    DiscussionSetFocusResult, ModelAskParams, Request, Response,
};

use crate::state::AppState;

pub async fn handle_model_ask(req: Request, state: &Arc<AppState>) -> Response {
    let params: ModelAskParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                tauqe_protocol::errors::INVALID_PARAMS,
                "Missing or invalid prompt",
            );
        }
    };

    use tauqe_flow::api::{AskError, DiscussionPromptError, ModelAskInput};
    let input = match params {
        ModelAskParams::Prompt { text } => ModelAskInput::Prompt { text },
        ModelAskParams::Discussion(request) => ModelAskInput::Discussion {
            target: request.target.project(),
            focused_items: request.focused_items,
            comment: request.comment,
        },
    };
    let observer = Arc::new(publication::Publisher {
        out: state.out.clone(),
    });
    match state.application.ask(input, observer).await {
        Ok(started) => Response::ok_typed(
            req.id,
            &serde_json::json!({
                "operation_id": started.operation_id,
            }),
        ),
        Err(error) => {
            use tauqe_protocol::errors::*;
            let code = match &error {
                AskError::Busy => OPERATION_IN_PROGRESS,
                AskError::Provider(_) => NO_API_KEY,
                AskError::Prompt(DiscussionPromptError::InvalidInput) => INVALID_PARAMS,
                AskError::Prompt(DiscussionPromptError::NotFound(_)) => NOT_FOUND,
                AskError::Prompt(DiscussionPromptError::Storage(_)) => INTERNAL_ERROR,
            };
            Response::err(req.id, code, error.to_string())
        }
    }
}

pub async fn handle_model_select(req: Request, state: &Arc<AppState>) -> Response {
    let params: tauqe_protocol::ModelSelectParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    tauqe_protocol::errors::INVALID_PARAMS,
                    "Missing or invalid model selection",
                );
            }
        };

    let observer = publication::Publisher {
        out: state.out.clone(),
    };
    let view = match state
        .application
        .change_model(params.selection.project(), &observer)
        .await
    {
        Ok(view) => view,
        Err(tauqe_flow::api::SettingsError::Busy) => {
            return Response::err(
                req.id,
                tauqe_protocol::errors::OPERATION_IN_PROGRESS,
                "Cannot change model while an operation is in progress",
            )
        }
        Err(error) => {
            return Response::err(
                req.id,
                tauqe_protocol::errors::INVALID_MODEL,
                error.to_string(),
            )
        }
    };
    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelSelectResult {
            selection: view.selection.project(),
            effective_model: view.model.project(),
            roles: Some(view.roles.project()),
        },
    )
}

pub async fn handle_model_current(req: Request, state: &Arc<AppState>) -> Response {
    let view = state.application.configuration_view().await;

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelCurrentResult {
            selection: view.selection.project(),
            effective_model: view.model.project(),
            roles: Some(view.roles.project()),
            available_models: view
                .available_models
                .into_iter()
                .map(crate::contracts::Project::project)
                .collect(),
        },
    )
}

/// Clears the persisted project lifetime usage totals.
///
/// Only the project scope is cleared: the session counters keep describing what
/// the running process has already spent, so a reset never makes the current
/// session appear free. The request is refused while an operation is active,
/// because the running turn flushes its own window on completion and would
/// immediately re-accumulate into the freshly cleared total.
///
/// The slot is sampled rather than claimed because the mutation is one bounded
/// write on the telemetry document (exclusion policy in [`crate::handlers`]): it
/// destroys only the lifetime totals it was asked to clear, and a turn that
/// registers afterwards contributes its own completed window on top of the
/// cleared floor instead of resurrecting a cleared record.
pub async fn handle_usage_reset(req: Request, state: &Arc<AppState>) -> Response {
    let observer = Arc::new(publication::Publisher {
        out: state.out.clone(),
    });
    match state.application.reset_project_usage(observer).await {
        Ok(snapshot) => Response::ok_typed(req.id, &crate::usage::wire_snapshot(&snapshot)),
        Err(_) => Response::err(
            req.id,
            tauqe_protocol::errors::OPERATION_IN_PROGRESS,
            "Cannot reset usage while a model operation is in progress",
        ),
    }
}

/// Sets the active discussion focus anchor and broadcasts the change.
pub async fn handle_discussion_set_focus(req: Request, state: &Arc<AppState>) -> Response {
    let params: DiscussionSetFocusParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    tauqe_protocol::errors::INVALID_PARAMS,
                    "Missing or invalid discussion focus",
                );
            }
        };
    let focus = state
        .application
        .set_focus(
            params.focus.project(),
            &publication::Publisher {
                out: state.out.clone(),
            },
        )
        .await
        .project();
    Response::ok_typed(req.id, &DiscussionSetFocusResult { focus })
}

/// Returns the active discussion focus anchor.
pub async fn handle_discussion_get_focus(req: Request, state: &Arc<AppState>) -> Response {
    let focus = state.application.discussion_focus().await.project();
    Response::ok_typed(req.id, &DiscussionGetFocusResult { focus })
}

/// Clears the active discussion focus anchor, returning the resulting focus.
pub async fn handle_discussion_clear_focus(req: Request, state: &Arc<AppState>) -> Response {
    let focus = state
        .application
        .set_focus(
            tauqe_flow::api::DiscussionFocus::None,
            &publication::Publisher {
                out: state.out.clone(),
            },
        )
        .await
        .project();
    Response::ok_typed(req.id, &DiscussionClearFocusResult { focus })
}

pub async fn handle_model_cancel(req: Request, state: &Arc<AppState>) -> Response {
    // The mode is optional so clients that predate two-mode cancellation keep
    // sending an empty object and receive the default immediate abort. A
    // present-but-malformed payload is refused instead of defaulted: silently
    // degrading to the default would answer a mistyped soft stop with the most
    // destructive cancellation, hard-cancelling a turn the client asked to
    // drain gracefully.
    let params: ModelCancelParams = match req.params {
        Some(params) => match serde_json::from_value(params) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    tauqe_protocol::errors::INVALID_PARAMS,
                    format!("Invalid cancel parameters: {}", err),
                );
            }
        },
        None => ModelCancelParams::default(),
    };

    cancel(req.id, state, params.mode)
}

pub(super) fn cancel(
    id: tauqe_protocol::RequestId,
    state: &AppState,
    mode: Option<CancelMode>,
) -> Response {
    use tauqe_flow::api::CancellationMode;
    let mode = match mode {
        Some(CancelMode::Soft) => CancellationMode::Soft,
        _ => CancellationMode::Hard,
    };
    state
        .application
        .cancel(mode, crate::reap::task_exit_deadline());
    Response::ok(id, serde_json::json!({"cancelled": true}))
}

pub async fn handle_model_clear_history(req: Request, state: &Arc<AppState>) -> Response {
    let observer = publication::Publisher {
        out: state.out.clone(),
    };
    match state.application.clear_dialogue_history(&observer).await {
        Ok(()) => Response::ok(req.id, serde_json::json!({"cleared": true})),
        Err(_) => Response::err(
            req.id,
            tauqe_protocol::errors::OPERATION_IN_PROGRESS,
            "Cannot clear history while a model operation is in progress",
        ),
    }
}

pub(crate) mod result;
