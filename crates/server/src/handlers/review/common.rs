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
    events, ContextAccess, IssueStatus, ReviewErrorEvent, ReviewIssue, ReviewListChangedEvent,
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

/// Persists a terminal review finding update and publishes the change.
///
/// Returns `Err` when the write does not land on disk — the storage error, or
/// a not-found diagnostic — so terminal `Fixed`/`Skipped` transitions surface
/// to the caller instead of letting the batch outcome diverge from the
/// persisted record. The failure is journaled here; the caller reports the
/// client-facing error under the operation it already owns, so no fresh
/// operation id is invented.
///
/// `what` names the transition for the failure diagnostic.
pub(super) async fn persist_finding_update<F>(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    what: &str,
    update: F,
) -> Result<(), String>
where
    F: FnOnce(&mut ReviewIssue) + Send + 'static,
{
    let save_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    let updated = offload_review(move || {
        ReviewStorage::update_session_item(&save_root, &review_id_owned, item_id, update)
    })
    .await;

    match updated {
        Ok(Some(_)) => {
            publish_review(state, repo_root, review_id).await;
            Ok(())
        }
        Ok(None) => {
            let message = format!(
                "Review item #{item_id} not found in review '{review_id}'; the {what} was not persisted"
            );
            tracing::warn!(review_id = review_id, item_id = item_id, "{}", message);
            Err(message)
        }
        Err(err) => {
            tracing::warn!(
                review_id = review_id,
                item_id = item_id,
                error = %err,
                "review finding update was not persisted"
            );
            Err(err)
        }
    }
}

/// Persists a new status for a review finding and publishes the change.
///
/// Best-effort variant: a persistence failure is journaled inside
/// [`set_item_status_checked`] and swallowed, which suits the `InProgress`
/// publication and the `Todo` reverts that accompany an already-reported
/// failure. Terminal transitions whose outcome must match the durable record
/// call the checked variant instead.
pub(super) async fn set_item_status(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    status: IssueStatus,
) {
    // This wrapper is the single sanctioned best-effort boundary: the failure
    // is already journaled by the checked variant, so discarding the result
    // here keeps the write observable without inventing a fresh operation id.
    let _ = set_item_status_checked(state, repo_root, review_id, item_id, status).await;
}

/// Persists a terminal review finding status and publishes the change.
///
/// Returns `Err` when the write does not land on disk, so the caller reports
/// the finding as failed instead of letting the batch outcome diverge from the
/// persisted state.
pub(super) async fn set_item_status_checked(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    status: IssueStatus,
) -> Result<(), String> {
    // `update_session_item_status` is `update_session_item` with a status
    // closure, so routing through the shared terminal-write helper keeps one
    // publication and failure-reporting path.
    let what = format!("'{status}' status update");
    persist_finding_update(state, repo_root, review_id, item_id, &what, move |it| {
        it.status = status;
    })
    .await
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
) -> Result<(), String> {
    // Terminal skip write: the caller must observe a persistence failure so
    // the batch outcome never diverges from the durable record.
    let notes = build_skip_note(assistant_text, reason);
    let persisted = persist_finding_update(
        state,
        repo_root,
        review_id,
        item_id,
        "Skipped status",
        move |it| {
            it.status = IssueStatus::Skipped;
            it.note = Some(notes);
        },
    )
    .await;

    if persisted.is_ok() {
        tracing::warn!(
            review_id = %review_id,
            item_id,
            reason = %reason.trim(),
            "review finding skipped after domain failure; batch continues"
        );
    }
    persisted
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
        // Declared finding files are never capped by an artificial quota, so both
        // the files budget and the refs budget stay unbounded here.
        let oc = apply_context_requests_detailed(
            ctx,
            &requests,
            &available_files,
            usize::MAX,
            usize::MAX,
        );
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

    /// Terminal finding writes must surface persistence failures: a skip whose
    /// `Skipped` record cannot be persisted returns `Err`, so the batch reports
    /// an infrastructure failure instead of advancing past a finding the
    /// durable record still shows as `InProgress`.
    #[tokio::test]
    async fn terminal_finding_writes_surface_persistence_failures() {
        // The fixture is process-unique and cleared before use: the test
        // corrupts its own session file, so a directory left behind by an
        // interrupted earlier run must never make `save_session` refuse the
        // session it is about to write.
        let root = std::env::temp_dir().join(format!(
            "tauqe-review-persist-{}-{}",
            std::process::id(),
            crate::state::next_operation_id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            root.clone(),
            history_sender,
            out,
        );

        let session = tauqe_protocol::ReviewSession {
            id: "rev-persist".to_string(),
            title: "Persistence Audit".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![tauqe_protocol::ReviewItem {
                id: 1,
                title: "Finding".to_string(),
                uuid: None,
                group: None,
                model: None,
                issue_type: tauqe_protocol::review::IssueType::Quality,
                severity: tauqe_protocol::review::ReviewSeverity::Suggestion,
                status: IssueStatus::InProgress,
                recommendations: vec![],
                files: vec![],
                readonly_files: vec![],
                resolution_notes: None,
                body: String::new(),
            }],
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
        };
        ReviewStorage::save_session(&root, &session).unwrap();

        // A healthy terminal write persists the skip and reports success.
        skip_finding_with_diagnostic(
            &state,
            &root,
            "rev-persist",
            1,
            "I could not patch the file safely.",
            "retry budget exhausted",
        )
        .await
        .expect("healthy skip write succeeds");
        let stored = ReviewStorage::load_review(&root, "rev-persist")
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Skipped);

        // A corrupted session file makes the session unresolvable, so the next
        // terminal write cannot land on disk: the failure must be returned,
        // not discarded. Review storage skips an unparseable session file and
        // reports the session as missing, so the surfaced diagnostic is the
        // not-persisted message rather than a raw parse error.
        let review_file = ReviewStorage::reviews_dir(&root).join("rev-persist.json");
        std::fs::write(&review_file, b"{ not a review session }").unwrap();
        let err = set_item_status_checked(&state, &root, "rev-persist", 1, IssueStatus::Todo)
            .await
            .expect_err("persistence failure must surface");
        assert!(err.contains("was not persisted"));
        assert!(err.contains("rev-persist"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
