//! Primary audit synthesis and cancellation handlers.
//!
//! Core filters and materializes the reviewer model's Structured Output, so
//! this adapter publishes only the decoded `summary` while raw JSON stays
//! inside the engine. Cancellation supports both a soft
//! stop (drain the current turn) and an immediate hard stop.

use crate::contracts::Project;
use std::sync::Arc;

use tauqe_core::api::{
    ResolvedReviewSynthesis, ReviewObserver, ReviewSynthesisExecutionConfig,
    ReviewSynthesisObserver, ReviewSynthesisPreparationError, UsageTotalsSnapshot,
};
use tauqe_core::providers::LlmProvider;
use tauqe_core::review::stream::ReviewStreamUpdate;
use tauqe_core::review::ReviewSession;
use tauqe_protocol::{
    errors, events, ModelUsageEvent, ModelUsageInfo, Request, Response, ReviewCancelParams,
    ReviewContentDeltaEvent, ReviewFinishedEvent, ReviewListChangedEvent,
    ReviewReasoningDeltaEvent, ReviewStartParams, ReviewStartedEvent, ReviewUpdatedEvent,
};

use crate::handlers::common::withdraw_streamed_attempt;
use crate::reap::task_exit_deadline;
use crate::state::{next_operation_id, AppState};

use super::common::{emit, emit_error};
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

    // A preset launch is a grouped audit: the session is materialized from
    // the configured preset and every group runs under the core batch
    // supervisor, so it never reads the interactive context window. The
    // plain context and pattern launch below keeps its live-streamed single
    // audit — the degenerate implicit `Default` group.
    let preset_name = params.preset.as_deref().map(str::trim);
    if let Some(preset_name) = preset_name.filter(|preset| !preset.is_empty()) {
        return start_preset_review(req, state, &params, preset_name.to_string()).await;
    }

    let resolved = state
        .application
        .resolve_review_synthesis(
            params
                .model
                .as_ref()
                .map(|value| value.clone().project())
                .as_ref(),
        )
        .await;
    let provider = match resolve_synthesis_provider(state, &resolved).await {
        Ok(provider) => provider,
        Err(error) => return Response::err(req.id, errors::NO_API_KEY, error),
    };
    let model = resolved.model.clone();
    let window = match state
        .application
        .prepare_review_synthesis_window(&resolved, &params.path_patterns)
        .await
    {
        Ok(window) => window,
        Err(error) => {
            let code = match &error {
                ReviewSynthesisPreparationError::NoPatternMatches(_) => errors::INVALID_PARAMS,
                ReviewSynthesisPreparationError::EmptyContext => errors::EMPTY_CONTEXT,
            };
            return Response::err(req.id, code, error.to_string());
        }
    };
    let repo_root = window.root;
    let files = window.files;
    let estimated_tokens = window.estimated_tokens;
    let omitted_files = window.omitted_by_budget;

    let operation = match state.try_begin_operation() {
        Ok(guard) => Arc::new(guard),
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    let op_id = format!("op-{}", next_operation_id());
    let prepared = match state.application.prepare_review_synthesis_request(
        &operation,
        &resolved,
        &files,
        params.user_prompt.clone(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            return Response::err(req.id, errors::OPERATION_IN_PROGRESS, error.to_string())
        }
    };

    emit(
        &state.out,
        events::REVIEW_STARTED,
        serde_json::to_value(ReviewStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone().project(),
            files_count: files.len(),
            estimated_tokens,
            omitted_files,
        })
        .ok(),
    );

    let state_for_spawn = Arc::clone(state);
    let op_id_for_spawn = op_id.clone();

    tokio::spawn(async move {
        let state = state_for_spawn;
        let op_id = op_id_for_spawn;
        let observer = Arc::new(ReviewSynthesisRpcObserver {
            state: Arc::clone(&state),
            operation_id: op_id.clone(),
            repo_root: repo_root.clone(),
        });
        let result = state
            .application
            .execute_review_synthesis(
                &operation,
                &repo_root,
                prepared,
                provider,
                observer,
                ReviewSynthesisExecutionConfig {
                    operation_id: op_id.clone(),
                    exit_deadline: task_exit_deadline(),
                },
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if error.is_cancelled() {
                    emit(
                        &state.out,
                        events::REVIEW_CANCELLED,
                        Some(serde_json::json!({ "operation_id": op_id })),
                    );
                } else {
                    emit_error(&state.out, &op_id, error.to_string());
                }
                return;
            }
        };
        let session = result.session;
        let operation_usage = crate::usage::wire_usage(&result.usage.prev.to_usage_info());
        let session_total_cost = result.usage.session.cost;
        let cost = result.usage.prev.cost;

        emit(
            &state.out,
            events::REVIEW_FINISHED,
            serde_json::to_value(ReviewFinishedEvent {
                operation_id: op_id,
                session: session.project(),
                usage: Some(operation_usage),
                session_total_cost: Some(session_total_cost),
                current_cost: Some(cost),
            })
            .ok(),
        );
    });

    Response::ok(req.id, serde_json::json!({ "operation_id": op_id }))
}
struct ReviewSynthesisRpcObserver {
    state: Arc<AppState>,
    operation_id: String,
    repo_root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl ReviewObserver for ReviewSynthesisRpcObserver {
    async fn pace(
        &self,
        event: &tauqe_core::providers::StreamEvent,
        cancel: &mut tokio::sync::watch::Receiver<bool>,
    ) {
        self.state.out.pace_event(event, cancel).await;
    }
    fn publish(&self, update: ReviewStreamUpdate) {
        match update {
            ReviewStreamUpdate::Reasoning(delta) => emit(
                &self.state.out,
                events::REVIEW_REASONING_DELTA,
                serde_json::to_value(ReviewReasoningDeltaEvent {
                    operation_id: self.operation_id.clone(),
                    delta,
                })
                .ok(),
            ),
            ReviewStreamUpdate::Content(delta) => emit(
                &self.state.out,
                events::REVIEW_CONTENT_DELTA,
                serde_json::to_value(ReviewContentDeltaEvent {
                    operation_id: self.operation_id.clone(),
                    delta,
                })
                .ok(),
            ),
            ReviewStreamUpdate::Reset => {
                withdraw_streamed_attempt(&self.state.out, &self.operation_id, || {})
            }
            _ => {}
        }
    }
    fn publish_usage_totals(&self, operation_id: &str, snapshot: &UsageTotalsSnapshot) {
        crate::usage::publish_snapshot(&self.state.out, operation_id, snapshot);
    }
    fn publish_usage(&self, snapshot: &UsageTotalsSnapshot) {
        let operation_usage: ModelUsageInfo =
            crate::usage::wire_usage(&snapshot.prev.to_usage_info());
        let session_total_cost = snapshot.session.cost;
        let cost = snapshot.prev.cost;
        emit(
            &self.state.out,
            events::MODEL_USAGE,
            serde_json::to_value(ModelUsageEvent {
                operation_id: self.operation_id.clone(),
                usage: operation_usage.clone(),
                session_total_cost,
                current_cost: Some(cost),
            })
            .ok(),
        );
    }
}

#[async_trait::async_trait]
impl ReviewSynthesisObserver for ReviewSynthesisRpcObserver {
    async fn publish_saved(&self, session: &ReviewSession) {
        emit(
            &self.state.out,
            events::REVIEW_UPDATED,
            serde_json::to_value(ReviewUpdatedEvent {
                session: session.clone().project(),
            })
            .ok(),
        );

        let list_root = self.repo_root.clone();
        if let Ok(reviews) = self
            .state
            .application
            .documents_at(&list_root)
            .reviews()
            .await
        {
            let active_root = self.repo_root.clone();
            let active_id = self
                .state
                .application
                .documents_at(&active_root)
                .active_review_id()
                .await
                .unwrap_or(None);
            emit(
                &self.state.out,
                events::REVIEW_LIST_CHANGED,
                serde_json::to_value(ReviewListChangedEvent {
                    reviews: reviews
                        .into_iter()
                        .map(crate::contracts::Project::project)
                        .collect(),
                    active_id,
                })
                .ok(),
            );
        }
    }
}

/// Preserve the scripted-provider seam; production setup belongs to core.
async fn resolve_synthesis_provider(
    _state: &Arc<AppState>,
    resolved: &ResolvedReviewSynthesis,
) -> Result<Arc<dyn LlmProvider>, String> {
    #[cfg(test)]
    if let Some(provider) = _state.provider_override.lock().await.clone() {
        return Ok(provider);
    }
    resolved.provider().map_err(|error| error.to_string())
}

/// Launches a grouped audit from a named review preset.
///
/// The preset is the single source of the session shape: its groups are
/// materialized with fresh identities, persisted as the immutable launch
/// snapshot, and audited one by one under the core batch supervisor. A
/// preset launch never reads the interactive context window, so the empty
/// context refusal of the plain launch does not apply here.
async fn start_preset_review(
    req: Request,
    state: &Arc<AppState>,
    params: &ReviewStartParams,
    preset_name: String,
) -> Response {
    let resolved = match state
        .application
        .resolve_review_preset(
            &preset_name,
            params
                .model
                .as_ref()
                .map(|value| value.clone().project())
                .as_ref(),
        )
        .await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            let code = match &error {
                tauqe_core::api::PresetReviewPreparationError::NotFound(_) => errors::NOT_FOUND,
                _ => errors::INVALID_PARAMS,
            };
            return Response::err(req.id, code, error.to_string());
        }
    };
    let root = state.repo_root().await;
    let operation = match state.try_begin_operation() {
        Ok(op) => Arc::new(op),
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            )
        }
    };
    let session = match state.application.prepare_preset_review(
        &operation,
        &resolved,
        params.user_prompt.clone(),
    ) {
        Ok(session) => session,
        Err(error) => return Response::err(req.id, errors::INVALID_PARAMS, error.to_string()),
    };
    let op_id = format!("op-{}", operation.id());
    emit(
        &state.out,
        events::REVIEW_STARTED,
        serde_json::to_value(ReviewStartedEvent {
            operation_id: op_id.clone(),
            model: session.model.clone().project(),
            files_count: 0,
            estimated_tokens: 0,
            omitted_files: 0,
        })
        .ok(),
    );
    let state = Arc::clone(state);
    let id = op_id.clone();
    tokio::spawn(async move {
        let observer = super::groups::observer(&state, &root, &id);
        match state
            .application
            .execute_preset_review(
                Arc::clone(&operation),
                &root,
                session,
                observer,
                task_exit_deadline(),
            )
            .await
        {
            Ok(result) => emit(
                &state.out,
                events::REVIEW_FINISHED,
                serde_json::to_value(ReviewFinishedEvent {
                    operation_id: id,
                    session: result.session.project(),
                    usage: Some(crate::usage::wire_usage(&result.usage)),
                    session_total_cost: Some(result.session_total_cost),
                    current_cost: Some(result.current_cost),
                })
                .ok(),
            ),
            Err(tauqe_core::api::PresetReviewExecutionError::Cancelled) => emit(
                &state.out,
                events::REVIEW_CANCELLED,
                Some(serde_json::json!({"operation_id":id})),
            ),
            Err(error) => emit_error(&state.out, &id, error.to_string()),
        }
        drop(operation);
    });
    Response::ok(req.id, serde_json::json!({"operation_id":op_id}))
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

    if params.mode == Some(tauqe_protocol::model::CancelMode::Soft) {
        state.soft_stop_active_operation();
        return Response::ok(req.id, serde_json::json!({ "cancelled": true }));
    }

    // A hard stop signals cancellation but keeps the operation slot registered:
    // the cancelled workflow keeps mutating state (rolling back its git
    // transaction, releasing the session lock) until it unwinds and drops its
    // guard. Clearing the slot immediately would let a client start
    // `git/undo`, `config/set`, or a new turn against a half-unwound turn. A
    // watchdog force-aborts a stalled task after the grace period.
    state.hard_cancel_active_operation();
    Response::ok(req.id, serde_json::json!({ "cancelled": true }))
}
