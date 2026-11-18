//! Model turn subsystem facade.
//!
//! Discussion prompt construction lives in [`prompt`] and the bounded turn
//! itself — provider resolution, protocol selection, streaming relay, and usage
//! accounting — lives in [`turn`]. This module exposes the RPC surface and the
//! turn primitives shared by the plan, review, architect, and locale handlers.

mod prompt;
pub(crate) mod turn;

#[cfg(test)]
mod tests;

use crate::contracts::Project;
use std::sync::Arc;

use tauqe_protocol::model::{CancelMode, ModelCancelParams};
use tauqe_protocol::{
    events, DiscussionClearFocusResult, DiscussionGetFocusResult, DiscussionSetFocusParams,
    DiscussionSetFocusResult, Event, ModelAskParams, Request, Response,
};

use crate::state::AppState;

pub use turn::start_model_turn;
pub(crate) use turn::unwind_grace;

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

    let resolved = match prompt::build_model_ask_prompt(state, params).await {
        Ok(resolved) => resolved,
        Err(err) => {
            return Response {
                id: req.id,
                result: None,
                error: Some(err),
            };
        }
    };

    match start_model_turn(state, resolved.prompt, resolved.focus, None, None).await {
        Ok(op_id) => Response {
            id: req.id,
            result: Some(serde_json::json!({ "operation_id": op_id })),
            error: None,
        },
        Err(err) => Response {
            id: req.id,
            result: None,
            error: Some(err),
        },
    }
}

pub async fn handle_model_select(req: Request, state: &Arc<AppState>) -> Response {
    // The mutation replaces the shared model selection and rotates the dialogue
    // session — the very state `config/set` protects by claiming the slot — so
    // the identical state change is guarded with the identical strength.
    // Sampling the slot once would let a turn register between the check and the
    // write and then run against a selection it never resolved.
    let operation = match crate::handlers::begin_configuration_change(
        &req.id,
        state,
        "Cannot change model while an operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

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

    let change = match operation.select_model(params.selection.project()).await {
        Ok(change) => change,
        Err(error) => {
            return Response::err(
                req.id,
                tauqe_protocol::errors::INVALID_MODEL,
                error.to_string(),
            )
        }
    };
    let selection = change.snapshot.selection;
    let effective_model = change.snapshot.config.resolve_model(&selection);
    let roles = crate::handlers::config::effective_roles_summary(&change.snapshot.config);

    state.out.send_event(&Event {
        method: events::MODEL_CHANGED.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelChangedEvent {
            selection: selection.clone().project(),
            effective_model: effective_model.clone().project(),
            roles: Some(roles.clone()),
        })
        .ok(),
    });

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelSelectResult {
            selection: selection.project(),
            effective_model: effective_model.project(),
            roles: Some(roles),
        },
    )
}

pub async fn handle_model_current(req: Request, state: &Arc<AppState>) -> Response {
    let guard = state.application.configuration().await;
    let selection = guard.selection.clone();
    let effective_model = guard.config.resolve_model(&selection);
    let available_models = guard.config.available_models();
    let roles = crate::handlers::config::effective_roles_summary(&guard.config);

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelCurrentResult {
            selection: selection.project(),
            effective_model: effective_model.project(),
            roles: Some(roles),
            available_models: available_models
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
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot reset usage while a model operation is in progress",
    ) {
        return resp;
    }

    let snapshot = state.reset_lifetime_usage().await;
    Response::ok_typed(req.id, &crate::usage::wire_snapshot(&snapshot))
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
    let focus = state.set_discussion_focus(params.focus).await;
    Response::ok_typed(req.id, &DiscussionSetFocusResult { focus })
}

/// Returns the active discussion focus anchor.
pub async fn handle_discussion_get_focus(req: Request, state: &Arc<AppState>) -> Response {
    let focus = state.discussion_focus().await;
    Response::ok_typed(req.id, &DiscussionGetFocusResult { focus })
}

/// Clears the active discussion focus anchor, returning the resulting focus.
pub async fn handle_discussion_clear_focus(req: Request, state: &Arc<AppState>) -> Response {
    let focus = state.clear_discussion_focus().await;
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

    // A soft stop is advisory: the batch supervisor finishes the current member
    // (its turn, compiler verification, and Git commit) and then stops before
    // launching the next one. The operation stays registered so its cancel and
    // soft-stop channels remain live until the batch unwinds on its own.
    if params.mode == Some(CancelMode::Soft) {
        state.soft_stop_active_operation();
        return Response {
            id: req.id,
            result: Some(serde_json::json!({ "cancelled": true })),
            error: None,
        };
    }

    // A hard stop signals cancellation but keeps the operation slot registered:
    // the cancelled workflow keeps mutating state (rolling back its git
    // transaction, releasing the session lock) until it observes the cancel
    // signal and drops its guard. Clearing the slot immediately would let a
    // client start `git/undo`, `config/set`, or a new turn against a half-unwound
    // turn. A watchdog force-aborts a stalled task after the grace period.
    state.hard_cancel_active_operation();
    Response {
        id: req.id,
        result: Some(serde_json::json!({ "cancelled": true })),
        error: None,
    }
}

pub async fn handle_model_clear_history(req: Request, state: &Arc<AppState>) -> Response {
    let operation = match crate::handlers::begin_history_change(
        &req.id,
        state,
        "Cannot clear history while a model operation is in progress",
    ) {
        Ok(operation) => operation,
        Err(response) => return *response,
    };
    let outcome = operation.clear().await;
    if let Some(error) = outcome.persistence_error {
        tracing::warn!(error = %error, "Failed to clear the interaction history");
    }
    state.publish_focus_update(outcome.focus_update);
    Response {
        id: req.id,
        result: Some(serde_json::json!({ "cleared": true })),
        error: None,
    }
}

mod result;
