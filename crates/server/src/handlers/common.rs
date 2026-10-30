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
use tokio::sync::watch;

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

/// Resolves as soon as an operation's cancellation signal is raised.
///
/// A batch supervisor races its step wait against this future, so a cancel
/// delivered in the middle of a step unwinds the queue immediately instead of
/// waiting for the step to finish its verification. The signal stays advisory
/// until it is raised: an already raised flag resolves at once, and a closed
/// channel keeps the future pending, because a dropped sender is not a
/// cancellation request.
pub(crate) async fn wait_for_cancellation(mut cancel_rx: watch::Receiver<bool>) {
    if *cancel_rx.borrow() {
        return;
    }
    while cancel_rx.changed().await.is_ok() {
        if *cancel_rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
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

/// Loads declared target and reference files into the read-only auto context
/// layer before an Architect synthesis, so the real code of every existing step
/// is visible on the first generation round. Declared target files are never
/// capped by the files token budget, matching the step-execution preload
/// contract.
pub(crate) async fn preload_readonly_files(
    state: &Arc<AppState>,
    repo_root: &Path,
    files: &[String],
) {
    let requests: Vec<ContextRequest> = files
        .iter()
        .map(String::as_str)
        .map(normalize_preload_path)
        .filter(|p| !p.is_empty())
        .map(|p| ContextRequest {
            path: p.to_string(),
            access: ContextAccess::ReadOnly,
        })
        .collect();
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
    use std::time::Duration;

    use super::*;

    /// The supervisors race a step against this future, so a signal raised
    /// while they wait must resolve it instead of waiting for the step.
    #[tokio::test]
    async fn wait_for_cancellation_resolves_when_the_signal_is_raised() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let waiter = tokio::spawn(wait_for_cancellation(cancel_rx));
        cancel_tx.send(true).unwrap();

        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the waiter observes the raised signal")
            .expect("the waiter task joins");
    }

    /// A signal raised before the supervision starts resolves at once, so a
    /// cancel delivered between the request and the wait is never lost.
    #[tokio::test]
    async fn wait_for_cancellation_resolves_for_an_already_raised_signal() {
        let (_cancel_tx, cancel_rx) = watch::channel(true);
        tokio::time::timeout(Duration::from_secs(5), wait_for_cancellation(cancel_rx))
            .await
            .expect("an already raised signal resolves immediately");
    }
}
