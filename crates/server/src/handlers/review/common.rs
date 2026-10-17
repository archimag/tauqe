//! Shared helpers for the review handler subsystem.
//!
//! This module holds the storage-offload wrapper, event emission, review
//! publication, status synchronization, and context preloading utilities that
//! every review submodule depends on. Concentrating them here keeps the
//! per-operation modules focused on orchestration instead of duplicating
//! plumbing, and keeps each module within the cohesive size target.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::context::ContextManager;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::review::prompt::format_review_step_failure_note;
use tauqe_core::review::ReviewStorage;
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    events, ContextAccess, Event, IssueStatus, ReviewErrorEvent, ReviewListChangedEvent,
    ReviewUpdatedEvent,
};

use crate::state::AppState;
use crate::transport::OutChannel;

/// Runs a blocking review-storage operation on the Tokio blocking pool.
///
/// The storage engine holds a synchronous process-wide mutex and performs disk
/// I/O, both of which must stay off the async worker threads.
pub(super) async fn offload_review<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match crate::handlers::offload(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(format!("{:#}", err)),
        Err(err) => Err(err),
    }
}

/// Emits a server event with an optional serialized parameter payload.
pub(super) fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
}

/// Emits a `review/error` event for a failed review operation.
pub(super) fn emit_error(out: &OutChannel, operation_id: &str, message: String) {
    emit(
        out,
        events::REVIEW_ERROR,
        serde_json::to_value(ReviewErrorEvent {
            operation_id: operation_id.to_string(),
            message,
        })
        .ok(),
    );
}

/// Publishes the updated review session and refreshed review list to the client.
pub(super) async fn publish_review(state: &Arc<AppState>, repo_root: &Path, review_id: &str) {
    let load_root = repo_root.to_path_buf();
    let load_id = review_id.to_string();
    if let Ok(Some(session)) =
        offload_review(move || ReviewStorage::load_review(&load_root, &load_id)).await
    {
        emit(
            &state.out,
            events::REVIEW_UPDATED,
            serde_json::to_value(ReviewUpdatedEvent { session }).ok(),
        );
    }
    let list_root = repo_root.to_path_buf();
    if let Ok(reviews) = offload_review(move || ReviewStorage::load_all(&list_root)).await {
        let active_root = repo_root.to_path_buf();
        let active_id = offload_review(move || ReviewStorage::load_active_id(&active_root))
            .await
            .ok()
            .flatten();
        emit(
            &state.out,
            events::REVIEW_LIST_CHANGED,
            serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
        );
    }
}

/// Persists a new status for a review finding and publishes the change.
pub(super) async fn set_item_status(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    status: IssueStatus,
) {
    let update_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    let updated = offload_review(move || {
        ReviewStorage::update_session_item_status(&update_root, &review_id_owned, item_id, status)
    })
    .await;

    if let Ok(Some(_)) = updated {
        publish_review(state, repo_root, review_id).await;
    }
}

/// Marks a finding as `Skipped` and records a domain failure diagnostic in its
/// resolution notes.
///
/// A domain failure is confined to the current finding: the batch supervisor
/// keeps running, and the recorded diagnostic lets the developer inspect why the
/// finding could not be resolved in code.
pub(super) async fn skip_finding_with_diagnostic(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    reason: &str,
) {
    let notes = format_review_step_failure_note(reason);
    let update_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    let notes_clone = notes.clone();
    let updated = offload_review(move || {
        ReviewStorage::update_session_item(&update_root, &review_id_owned, item_id, move |it| {
            it.status = IssueStatus::Skipped;
            it.note = Some(notes_clone);
        })
    })
    .await;

    if let Ok(Some(_)) = updated {
        tracing::warn!(
            review_id = %review_id,
            item_id,
            reason = %reason.trim(),
            "review finding skipped after domain failure; batch continues"
        );
        publish_review(state, repo_root, review_id).await;
    }
}

/// Emits a `model/error` event for a failed review finding.
pub(super) fn report_item_failure(state: &Arc<AppState>, operation_id: &str, message: &str) {
    emit(
        &state.out,
        events::MODEL_ERROR,
        Some(serde_json::json!({
            "operation_id": operation_id,
            "message": message,
        })),
    );
}

/// Runs a context mutation on the blocking pool without holding the async
/// context guard across disk I/O.
///
/// The context manager is cloned into a private snapshot under a short-lived
/// read guard, mutated inside the blocking pool, and written back under a
/// short-lived write guard. This keeps `state.context` free while `std::fs`
/// metadata reads run, so the async worker is never blocked by the mutation
/// (and never starves on the guard while a sibling task holds it).
pub(super) async fn with_context_mutation<T, F>(state: &Arc<AppState>, f: F) -> Result<T, String>
where
    F: FnOnce(&mut ContextManager) -> T + Send + 'static,
    T: Send + 'static,
{
    let snapshot = state.context.read().await.clone();
    let (result, mutated) = crate::handlers::offload(move || {
        let mut ctx = snapshot;
        let result = f(&mut ctx);
        (result, ctx)
    })
    .await?;

    {
        let mut guard = state.context.write().await;
        *guard = mutated;
    }
    Ok(result)
}

/// Loads the target finding's files into the read-only auto context layer before
/// the architect begins decomposition, guaranteeing baseline visibility of the
/// affected code on the first generation round. Declared target files are never
/// capped by the files token budget.
pub(super) async fn preload_refine_item_files(
    state: &Arc<AppState>,
    repo_root: &Path,
    files: &[String],
) {
    let requests: Vec<ContextRequest> = files
        .iter()
        .map(|f| f.trim().trim_start_matches("./").trim_start_matches('/'))
        .filter(|p| !p.is_empty())
        .map(|p| ContextRequest {
            path: p.to_string(),
            access: ContextAccess::ReadOnly,
        })
        .collect();
    if requests.is_empty() {
        return;
    }
    let available_files = {
        let repo_root = repo_root.to_path_buf();
        crate::handlers::offload(move || tauqe_core::git::list_repository_files(Some(&repo_root)))
            .await
            .ok()
            .and_then(|files| files.ok())
            .unwrap_or_default()
    };

    let outcome = with_context_mutation(state, move |ctx| {
        let oc = apply_context_requests_detailed(ctx, &requests, &available_files, usize::MAX);
        let s = ctx.get_state();
        (oc, s)
    })
    .await;

    if let Ok((outcome, new_ctx_state)) = outcome {
        if !outcome.added.is_empty() {
            emit(
                &state.out,
                events::CONTEXT_CHANGED,
                Some(serde_json::json!({ "state": new_ctx_state })),
            );
        }
    }
}

/// Loads all files referenced by a finding into editable context before the turn
/// starts. Declared finding files are never capped by the files token budget.
pub(super) async fn preload_item_files(state: &Arc<AppState>, repo_root: &Path, files: &[String]) {
    let requests: Vec<ContextRequest> = files
        .iter()
        .map(|f| f.trim().trim_start_matches("./").trim_start_matches('/'))
        .filter(|p| !p.is_empty())
        .map(|p| ContextRequest {
            path: p.to_string(),
            access: ContextAccess::Editable,
        })
        .collect();
    if requests.is_empty() {
        return;
    }
    let available_files = {
        let repo_root = repo_root.to_path_buf();
        crate::handlers::offload(move || tauqe_core::git::list_repository_files(Some(&repo_root)))
            .await
            .ok()
            .and_then(|files| files.ok())
            .unwrap_or_default()
    };

    let outcome = with_context_mutation(state, move |ctx| {
        let oc = apply_context_requests_detailed(ctx, &requests, &available_files, usize::MAX);
        let s = ctx.get_state();
        (oc, s)
    })
    .await;

    if let Ok((outcome, new_ctx_state)) = outcome {
        if !outcome.added.is_empty() {
            emit(
                &state.out,
                events::CONTEXT_CHANGED,
                Some(serde_json::json!({ "state": new_ctx_state })),
            );
        }
    }
}

/// Clears the transient auto context layer so a retried attempt starts from a
/// clean, deterministic basis, then notifies clients of the new snapshot.
pub(super) async fn clear_auto_context(state: &Arc<AppState>) {
    let cleared = with_context_mutation(state, |ctx| {
        ctx.clear_auto();
        ctx.get_state()
    })
    .await;

    if let Ok(new_ctx_state) = cleared {
        emit(
            &state.out,
            events::CONTEXT_CHANGED,
            Some(serde_json::json!({ "state": new_ctx_state })),
        );
    }
}

/// Whether a completed turn actually applied code edits to disk.
///
/// Review findings ask the model to fix code: explanatory text alone, even when
/// accompanied by a terminal success tag, must not satisfy the finding contract.
pub(super) fn turn_applied_edits(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) => {
            outcome.applied && outcome.error.is_none()
        }
        _ => false,
    }
}
