//! Shared helpers for the RPC handler subsystems.
//!
//! Event emission, transient auto-context management, turn-outcome inspection,
//! read-only context preloading, and Architect synthesis history recording are
//! identical across the plan, review, and architect handlers. They are defined
//! once here so every subsystem emits events and mutates context through a
//! single code path.

use std::path::Path;
use std::sync::Arc;

use crate::state::AppState;
use crate::transport::OutChannel;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::discussion::DiscussionTarget;
use tauqe_protocol::{events, ContextAccess, Event};

/// Emits a server event with an optional serialized parameter payload.
pub(crate) fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
}

/// Reason published with every withdrawal of an aborted stream attempt.
const STREAM_RESET_REASON: &str = "connection interrupted mid-stream";

/// Withdraws every fragment an aborted provider attempt already streamed.
///
/// A retrying provider re-issues a request that broke mid-stream, and the retry
/// re-generates the answer from its beginning. The fragments of the aborted
/// attempt are therefore void: they must not survive in the contour's own
/// buffer, and they must not survive in the client, or the retried payload is
/// concatenated onto a prefix that belongs to a different attempt.
///
/// Each relaying contour owns the buffers it accumulated, so it passes their
/// clearing here as `discard`; the single reset event published afterwards is
/// what makes the client drop its own copy. Expressing the withdrawal once keeps
/// the interactive turn, the Architect stream, and the review synthesis from
/// drifting apart.
pub(crate) fn withdraw_streamed_attempt(
    out: &OutChannel,
    operation_id: &str,
    discard: impl FnOnce(),
) {
    discard();
    emit(
        out,
        events::MODEL_STREAM_RESET,
        serde_json::to_value(tauqe_protocol::ModelStreamResetEvent {
            operation_id: operation_id.to_string(),
            reason: STREAM_RESET_REASON.to_string(),
        })
        .ok(),
    );
}

/// Whether a completed turn actually applied code edits to disk.
///
/// Plan steps and review findings ask the model to modify code: explanatory text
/// alone, even when accompanied by a terminal success tag, must not satisfy the
/// contract.
pub(crate) fn turn_applied_edits(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) => {
            outcome.applied && outcome.error.is_none()
        }
        _ => false,
    }
}

/// Whether a completed turn left changes in the working tree that the failed
/// attempt never reverted.
///
/// A failed edit turn rolls its Git transaction back to the step checkpoint, and
/// the FSM empties `changed_files` once that rollback succeeded. A change set the
/// FSM refused to apply while still naming modified files is therefore the single
/// signal that the working tree carries the bytes of an aborted attempt: an empty
/// change set never touched disk, and a turn without an edit result owns no
/// change set at all. A supervisor ends the step with a recovery instruction on
/// this signal instead of retrying on a dirty tree.
pub fn turn_left_unreverted_changes(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) => {
            !outcome.applied && !outcome.changed_files.is_empty()
        }
        _ => false,
    }
}

/// Releases the active discussion focus when it anchors the deleted entity.
///
/// A focus anchor may never address a non-existent entity, so deleting a plan
/// or a review session releases a focus bound to the removed target together
/// with it. The release goes through the shared [`AppState`] facade, which
/// broadcasts `DISCUSSION_FOCUS_CHANGED`, so every client drops the dead
/// anchor instead of rendering it. A focus anchored on any other entity is
/// left untouched. The single shared implementation keeps plan and review
/// deletions from drifting apart.
pub(crate) async fn release_focus_anchored_on(state: &Arc<AppState>, anchor: &DiscussionTarget) {
    if state.discussion_focus().await.target() == Some(anchor) {
        state.clear_discussion_focus().await;
        tracing::info!(
            anchor = ?anchor,
            "Cleared the discussion focus anchored on the deleted entity"
        );
    }
}

/// Clears the transient auto context layer so a retried attempt starts from a
/// clean, deterministic basis, then notifies clients of the new snapshot.
///
/// The clear goes through the shared [`AppState`] facade, so removing files from
/// the transient layer also rotates the provider session identifier when the
/// layer really held files: the stable prompt prefix those files warmed no
/// longer exists, and the next round must not be routed to a node holding a
/// stale copy of it.
pub(crate) async fn clear_auto_context(state: &Arc<AppState>) {
    if let Ok(new_ctx_state) = state.clear_auto_context().await {
        emit(
            &state.out,
            events::CONTEXT_CHANGED,
            Some(serde_json::json!({ "state": new_ctx_state })),
        );
    }
}

/// Normalizes a declared plan file path into the repository-relative form used
/// for context preloading, trimming whitespace and stripping leading `./` and
/// `/` markers so cosmetic spelling differences collapse to one key.
pub(crate) fn normalize_preload_path(path: &str) -> &str {
    path.trim().trim_start_matches("./").trim_start_matches('/')
}

/// Lists the tracked repository files on the blocking pool.
///
/// The listing spawns a `git` process, so it is resolved before any exclusive
/// context guard is taken: a guarded mutation closure must never hold every
/// context reader behind a process spawn.
pub(crate) async fn load_repository_files(repo_root: &Path) -> Vec<String> {
    let root = repo_root.to_path_buf();
    crate::handlers::offload(move || tauqe_core::git::list_repository_files(Some(&root)))
        .await
        .ok()
        .and_then(|files| files.ok())
        .unwrap_or_default()
}

/// Loads declared context files into the transient auto layer before a turn.
///
/// Target files are registered as editable and reference files as read-only.
/// Declared files are never capped by an artificial quota: the caller that
/// declares them already decided they are required, so both the files budget and
/// the refs budget stay unbounded. One implementation serves the step turn of the
/// implementation host and the read-only preload of an Architect synthesis, so the
/// two surfaces cannot drift apart.
pub(crate) async fn preload_declared_files(
    state: &Arc<AppState>,
    repo_root: &Path,
    files: &[String],
    readonly_files: &[String],
) {
    let mut requests: Vec<ContextRequest> = Vec::with_capacity(files.len() + readonly_files.len());
    requests.extend(
        files
            .iter()
            .map(String::as_str)
            .map(normalize_preload_path)
            .filter(|p| !p.is_empty())
            .map(|p| ContextRequest {
                path: p.to_string(),
                access: ContextAccess::Editable,
            }),
    );
    requests.extend(
        readonly_files
            .iter()
            .map(String::as_str)
            .map(normalize_preload_path)
            .filter(|p| !p.is_empty())
            .map(|p| ContextRequest {
                path: p.to_string(),
                access: ContextAccess::ReadOnly,
            }),
    );
    if requests.is_empty() {
        return;
    }

    let available_files = load_repository_files(repo_root).await;
    let preloaded = crate::handlers::with_context_mutation(state, move |ctx| {
        // Declared preload files are never capped by an artificial quota, so both
        // the files budget and the refs budget stay unbounded here.
        let outcome = apply_context_requests_detailed(
            ctx,
            &requests,
            &available_files,
            usize::MAX,
            usize::MAX,
        );
        let new_state = ctx.get_state();
        (outcome, new_state)
    })
    .await;

    if let Ok((outcome, new_ctx_state)) = preloaded {
        if !outcome.added.is_empty() {
            emit(
                &state.out,
                events::CONTEXT_CHANGED,
                Some(serde_json::json!({ "state": new_ctx_state })),
            );
        }
    }
}

/// Loads declared target and reference files into the read-only auto context
/// layer before an Architect synthesis, so the real code of every existing step
/// is visible on the first generation round.
pub(crate) async fn preload_readonly_files(
    state: &Arc<AppState>,
    repo_root: &Path,
    files: &[String],
) {
    preload_declared_files(state, repo_root, &[], files).await;
}

/// Appends the developer directives section to an execution prompt when present.
///
/// Every prompt contour that can be started with developer directives — a plan
/// step, a review finding, a review sub-step — appends them the same way, so a
/// directive section never drifts between the workflows that accept one.
pub(crate) fn append_developer_directives(prompt: &mut String, instructions: Option<&str>) {
    if let Some(dirs) = instructions.map(str::trim).filter(|d| !d.is_empty()) {
        prompt.push_str("\n## Developer Directives:\n");
        prompt.push_str(dirs);
        prompt.push('\n');
    }
}

/// Records an Architect synthesis interaction atomically once it reaches a
/// terminal outcome.
///
/// The user turn and its response are written together under a single history
/// mutation, only after the synthesis has produced a durable result. Recording
/// the turn before the discovery loop would leave an orphaned user entry in the
/// transcript whenever the round is cancelled, fails, or is rejected before a
/// response exists. Persistence is `fsync`-bounded, so it runs on the blocking
/// pool through `crate::handlers::with_history_mutation` instead of stalling an
/// async worker thread.
pub(crate) async fn record_synthesis_history(
    state: &Arc<AppState>,
    turn_prompt: String,
    context_paths: Vec<String>,
    response_message: String,
    response_summary: Option<String>,
) {
    let _ = crate::handlers::with_history_mutation(state, move |history| {
        let _ = history.record_turn(turn_prompt, context_paths);
        let _ = history.record_response(response_message, None, response_summary, Vec::new());
    })
    .await;
}

/// Test-only helpers shared by the handler regression suites.
///
/// They live beside the production helpers so a read-path regression test does
/// not re-implement scratch-repository setup or tree snapshots in every
/// subsystem.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// Creates a unique scratch repository root for a handler regression test.
    pub(crate) fn scratch_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "tauqe-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create the scratch repository root");
        root
    }

    /// Snapshots every file under `root` as `(relative path, bytes)`.
    ///
    /// Comparing two snapshots proves that a request left the persisted state
    /// byte-identical — no file created, rewritten, or removed — without
    /// hard-coding the storage layout of the subsystem under test.
    pub(crate) fn snapshot_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut snapshot = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let relative = path.strip_prefix(root).map_or_else(
                    |_| path.to_string_lossy().into_owned(),
                    |relative| relative.to_string_lossy().replace('\\', "/"),
                );
                snapshot.push((relative, std::fs::read(&path).unwrap_or_default()));
            }
        }
        snapshot.sort();
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::model::ModelTurnResult;

    /// Builds a completed turn carrying the given model result.
    fn completed_turn(result: Option<tauqe_protocol::ModelResult>) -> ModelTurnResult {
        ModelTurnResult {
            operation_id: "op-test".to_string(),
            result,
            is_cancelled: false,
            error: None,
            has_terminal_success: false,
            blocked: None,
            abort: None,
        }
    }

    /// Builds an edit result with the given verdict and reported files.
    fn edit_result(applied: bool, changed_files: Vec<String>) -> tauqe_protocol::ModelResult {
        tauqe_protocol::ModelResult::Edit {
            summary: "edit".to_string(),
            edits: Vec::new(),
            proposal: None,
            outcome: tauqe_protocol::EditOutcome {
                applied,
                error: if applied {
                    None
                } else {
                    Some("edit rejected".to_string())
                },
                changed_files,
                commit_hash: None,
                verification: None,
            },
        }
    }

    #[test]
    fn test_turn_left_unreverted_changes_reports_only_a_change_that_reached_disk() {
        // An applied change set went through the turn's own transaction.
        let applied = completed_turn(Some(edit_result(true, vec!["src/lib.rs".to_string()])));
        assert!(!turn_left_unreverted_changes(&applied));

        // A rejected change set that rolled back cleanly reports no files: the
        // FSM empties `changed_files` once the transaction is restored.
        let clean = completed_turn(Some(edit_result(false, Vec::new())));
        assert!(!turn_left_unreverted_changes(&clean));

        // A rejected change set that still names modified files means the
        // rollback never reached the step checkpoint.
        let dirty = completed_turn(Some(edit_result(false, vec!["src/lib.rs".to_string()])));
        assert!(turn_left_unreverted_changes(&dirty));

        // A conversational answer touches no bytes.
        let answer = completed_turn(Some(tauqe_protocol::ModelResult::Answer {
            text: "Nothing to change.".to_string(),
        }));
        assert!(!turn_left_unreverted_changes(&answer));

        // A turn that produced no result at all owns no change set.
        let no_result = completed_turn(None);
        assert!(!turn_left_unreverted_changes(&no_result));
    }
}
