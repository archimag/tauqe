//! Model turn subsystem facade.
//!
//! Discussion prompt construction lives in [`prompt`] and the bounded turn
//! itself — provider resolution, protocol selection, streaming relay, and usage
//! accounting — lives in [`turn`]. This module exposes the RPC surface and the
//! turn primitives shared by the plan, review, architect, and locale handlers.

mod prompt;
mod turn;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use tauqe_protocol::model::{CancelMode, ModelCancelParams};
use tauqe_protocol::{
    events, DiscussionClearFocusResult, DiscussionGetFocusResult, DiscussionSetFocusParams,
    DiscussionSetFocusResult, Event, ModelAskParams, Request, Response,
};

use crate::state::AppState;

pub use turn::{execute_model_turn, start_model_turn, ModelTurnResult, StepTurnOptions};
pub(crate) use turn::{build_apply_policy, emit_legacy_usage_event, CANCEL_UNWIND_GRACE};

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

    let (prompt, intent) = match prompt::build_model_ask_prompt(state, params).await {
        Ok(resolved) => resolved,
        Err(err) => {
            return Response {
                id: req.id,
                result: None,
                error: Some(err),
            };
        }
    };

    match start_model_turn(state, prompt, intent, None, None).await {
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
    if state.is_operation_active() {
        return Response::err(
            req.id,
            tauqe_protocol::errors::OPERATION_IN_PROGRESS,
            "Cannot change model while an operation is in progress",
        );
    }

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

    let (selection, effective_model, roles) = {
        let mut guard = state.config.lock().await;
        // Restrict the selection to the configured model pools: a role resolves
        // to its base model and an explicit model must already be configured,
        // so a client can only activate a model it was offered.
        let effective = guard.config.resolve_model(&params.selection);
        let available = guard.config.available_models();
        if !available.contains(&effective) {
            return Response::err(
                req.id,
                tauqe_protocol::errors::INVALID_MODEL,
                format!(
                    "Unknown model '{}'. Available models: {}",
                    effective,
                    available
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        guard.selection = params.selection.clone();
        let roles = crate::handlers::config::effective_roles_summary(&guard.config);
        (guard.selection.clone(), effective, roles)
    };

    state.out.send_event(&Event {
        method: events::MODEL_CHANGED.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelChangedEvent {
            selection: selection.clone(),
            effective_model: effective_model.clone(),
            roles: Some(roles.clone()),
        })
        .ok(),
    });

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelSelectResult {
            selection,
            effective_model,
            roles: Some(roles),
        },
    )
}

pub async fn handle_model_current(req: Request, state: &Arc<AppState>) -> Response {
    let guard = state.config.lock().await;
    let selection = guard.selection.clone();
    let effective_model = guard.config.resolve_model(&selection);
    let available_models = guard.config.available_models();
    let roles = crate::handlers::config::effective_roles_summary(&guard.config);

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelCurrentResult {
            selection,
            effective_model,
            roles: Some(roles),
            available_models,
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
pub async fn handle_usage_reset(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot reset usage while a model operation is in progress",
    ) {
        return resp;
    }

    let snapshot = state.reset_lifetime_usage().await;
    Response::ok_typed(req.id, &snapshot)
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
    // sending an empty object and receive the default immediate abort.
    let params: ModelCancelParams = req
        .params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();

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
    // Clearing history while a turn is compacting or recording it would race
    // the running operation, so the mutation is refused for its duration.
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot clear history while a model operation is in progress",
    ) {
        return resp;
    }

    // Clearing the history truncates `history.jsonl` behind an `fsync` barrier,
    // so the mutation runs on the blocking pool with the exclusive history
    // guard held instead of stalling an async worker.
    let cleared = crate::handlers::with_history_mutation(state, |history| history.clear()).await;
    if let Err(err) = cleared {
        tracing::warn!(error = %err, "Failed to clear the interaction history");
    }
    // Clearing the dialogue session also detaches the discussion focus so the
    // REPL returns to the free, unfocused mode, and rotates the provider session
    // identifier so the discarded transcript can never warm the prompt-prefix
    // cache of the next dialogue.
    state.clear_discussion_focus().await;
    state.reset_session_uuid().await;
    Response {
        id: req.id,
        result: Some(serde_json::json!({ "cleared": true })),
        error: None,
    }
}
