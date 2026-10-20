//! Shared helpers for the RPC handler subsystems.
//!
//! Event emission, transient auto-context management, turn-outcome inspection,
//! read-only context preloading, and Architect synthesis history recording are
//! identical across the plan, review, and architect handlers. They are defined
//! once here so every subsystem emits events and mutates context through a
//! single code path.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{events, ContextAccess, Event};

use crate::state::AppState;
use crate::transport::OutChannel;

/// Emits a server event with an optional serialized parameter payload.
pub(crate) fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
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

/// Clears the transient auto context layer so a retried attempt starts from a
/// clean, deterministic basis, then notifies clients of the new snapshot.
pub(crate) async fn clear_auto_context(state: &Arc<AppState>) {
    let cleared = crate::handlers::with_context_mutation(state, |ctx| {
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

/// Normalizes a declared plan file path into the repository-relative form used
/// for context preloading, trimming whitespace and stripping leading `./` and
/// `/` markers so cosmetic spelling differences collapse to one key.
pub(crate) fn normalize_preload_path(path: &str) -> &str {
    path.trim().trim_start_matches("./").trim_start_matches('/')
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
    let root = repo_root.to_path_buf();
    let preloaded = crate::handlers::with_context_mutation(state, move |ctx| {
        let available_files = tauqe_core::git::list_repository_files(Some(&root))
            .ok()
            .unwrap_or_default();
        let outcome = apply_context_requests_detailed(ctx, &requests, &available_files, usize::MAX);
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
