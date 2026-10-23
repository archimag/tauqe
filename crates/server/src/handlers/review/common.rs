//! Shared helpers for the review handler subsystem.
//!
//! This module holds the storage-offload wrapper, event emission, review
//! publication, status synchronization, and context preloading utilities that
//! every review submodule depends on. Concentrating them here keeps the
//! per-operation modules focused on orchestration instead of duplicating
//! plumbing, and keeps each module within the cohesive size target.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::review::prompt::format_review_step_failure_note;
use tauqe_core::review::ReviewStorage;
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    events, ContextAccess, IssueStatus, ReviewErrorEvent, ReviewListChangedEvent,
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

/// Event emission, auto-context clearing, and turn-outcome inspection are shared
/// with every other handler subsystem, so they are re-exported here instead of
/// being duplicated per subsystem.
pub(super) use crate::handlers::common::{clear_auto_context, emit, turn_applied_edits};

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

/// Maximum number of Unicode scalar values retained from the model's final
/// comment when a finding is skipped, so a long response never bloats the note.
const SKIP_COMMENT_MAX_CHARS: usize = 2000;

/// Appends the model's final comment to a base diagnostic, truncating the
/// comment on a Unicode scalar boundary.
///
/// The comment is truncated with `chars().take(..)`, never with a raw byte
/// offset, so multibyte text (Cyrillic, CJK, emoji) stays intact and can never
/// trigger a `byte index is not a char boundary` panic. An empty comment leaves
/// the base diagnostic untouched.
pub(super) fn append_model_comment(base: String, assistant_text: &str) -> String {
    let comment = assistant_text.trim();
    if comment.is_empty() {
        return base;
    }
    let total_chars = comment.chars().count();
    let truncated: String = comment.chars().take(SKIP_COMMENT_MAX_CHARS).collect();
    if total_chars > SKIP_COMMENT_MAX_CHARS {
        format!("{}\n\n## Model Response (truncated):\n{}…", base, truncated)
    } else {
        format!("{}\n\n## Model Response:\n{}", base, truncated)
    }
}

/// Builds the `resolution_notes` diagnostic recorded when a finding is skipped.
///
/// The diagnostic combines the failure reason with the model's final response, so
/// the developer can inspect both why the finding could not be resolved and what
/// the model reported as it gave up.
fn build_skip_note(assistant_text: &str, reason: &str) -> String {
    append_model_comment(format_review_step_failure_note(reason), assistant_text)
}

/// Marks a finding as `Skipped` and records a domain failure diagnostic in its
/// resolution notes.
///
/// A domain failure is confined to the current finding: the batch supervisor
/// keeps running, and the recorded diagnostic lets the developer inspect why the
/// finding could not be resolved in code. The model's final comment is preserved
/// alongside the reason whenever it is available.
pub(super) async fn skip_finding_with_diagnostic(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    assistant_text: &str,
    reason: &str,
) {
    let notes = build_skip_note(assistant_text, reason);
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

/// Runs a context mutation atomically under the exclusive context write lock.
///
/// The single implementation lives in `crate::handlers`; re-exporting it here
/// keeps review-side call sites unchanged while removing the duplicated body
/// that cloned the manager and overwrote the shared state afterwards.
pub(super) use crate::handlers::with_context_mutation;

/// Re-exported under the review-side name so existing call sites stay stable
/// while the loading logic is defined once in `crate::handlers::common`.
///
/// Loads the target finding's files into the read-only auto context layer before
/// the architect begins decomposition, guaranteeing baseline visibility of the
/// affected code on the first generation round. Declared target files are never
/// capped by the files token budget.
pub(super) use crate::handlers::common::preload_readonly_files as preload_refine_item_files;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_note_keeps_comment_and_reason() {
        let note = build_skip_note(
            "I could not patch the file safely.",
            "retry budget exhausted",
        );
        assert!(note.contains("retry budget exhausted"));
        assert!(note.contains("I could not patch the file safely."));
        assert!(note.contains("## Model Response:"));
        assert!(!note.contains("(truncated)"));
    }

    #[test]
    fn skip_note_without_comment_falls_back_to_reason() {
        let note = build_skip_note("   ", "compiler errors persisted");
        assert!(note.contains("compiler errors persisted"));
        assert!(!note.contains("## Model Response"));
    }

    #[test]
    fn append_model_comment_preserves_base_and_comment() {
        let note = append_model_comment(
            "Subplan interrupted before completion: patch failed".to_string(),
            "I stopped after the second hunk failed.",
        );
        assert!(note.contains("Subplan interrupted before completion: patch failed"));
        assert!(note.contains("I stopped after the second hunk failed."));
    }

    #[test]
    fn skip_note_truncates_on_utf8_boundary() {
        let text: String = "я".repeat(SKIP_COMMENT_MAX_CHARS + 500);
        let note = build_skip_note(&text, "retry budget exhausted");
        assert!(note.contains("## Model Response (truncated):"));
        let comment_part = note
            .split_once("## Model Response (truncated):\n")
            .map(|(_, rest)| rest)
            .expect("truncated marker present");
        let trailing = comment_part
            .strip_suffix('…')
            .expect("ellipsis suffix present");
        assert_eq!(trailing.chars().count(), SKIP_COMMENT_MAX_CHARS);
        assert!(trailing.chars().all(|c| c == 'я'));
    }
}
