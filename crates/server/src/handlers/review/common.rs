//! Shared helpers for the review handler subsystem.
//!
//! Review execution owns no status transition of its own. Every step turn,
//! every Architect wave, and every durable finding transition belongs to the
//! engine's unified implementation pipeline, which settles both a group horizon
//! and a degenerate finding run through `ReviewStorage`. What remains here is
//! the plumbing every review submodule depends on: the blocking storage
//! offload, event emission, and review publication. Concentrating them once
//! keeps the per-operation modules focused on orchestration and keeps each of
//! them within the cohesive size target.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{events, ReviewErrorEvent, ReviewListChangedEvent, ReviewUpdatedEvent};

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

/// Event emission and auto-context clearing are shared with every other handler
/// subsystem, so they are re-exported here instead of being duplicated per
/// subsystem.
pub(super) use crate::handlers::common::{clear_auto_context, emit};

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
///
/// The publication is shared with the dialogue turn: a dialogue anchored on a
/// review session records the decision the engineer reached, so the refreshed
/// session reaches every client through this single path.
pub(crate) async fn publish_review(state: &Arc<AppState>, repo_root: &Path, review_id: &str) {
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
