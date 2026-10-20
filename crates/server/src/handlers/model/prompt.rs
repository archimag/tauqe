//! Discussion prompt construction for model turns.
//!
//! A structured discussion turn and a focused interactive turn are both built
//! here: the typed discussion target decides the prompt text and the turn
//! intent, so neither is ever inferred from the prompt body.

use std::sync::Arc;

use tauqe_protocol::{
    DiscussionFocus, DiscussionRequest, DiscussionTarget, ModelAskParams, ResponseError, TurnIntent,
};

use crate::handlers::offload;
use crate::state::AppState;

/// Resolves the turn prompt and its declared intent for a model ask request.
///
/// A structured discussion turn carries its target explicitly, so the server
/// generates the discussion prompt from those typed fields and replaces the
/// focus anchor. A plain ask continues the anchored discussion instead: while a
/// focus is active, the current state of the focused plan or review session is
/// injected into the prompt on every turn. The intent is derived from the
/// resolved target rather than from the prompt text, so text that merely
/// resembles a discussion heading can never switch the turn mode.
pub(super) async fn build_model_ask_prompt(
    state: &Arc<AppState>,
    params: ModelAskParams,
) -> Result<(String, TurnIntent), ResponseError> {
    match params {
        ModelAskParams::Discussion(discussion) => {
            let prompt = build_discussion_prompt(
                state,
                &discussion.target,
                &discussion.focused_items,
                &discussion.comment,
            )
            .await?;
            // An explicit discussion target immediately replaces the focus
            // anchor without resetting the REPL history.
            state
                .set_discussion_focus(focus_from_target(&discussion))
                .await;
            Ok((prompt, intent_for_target(&discussion.target)))
        }
        ModelAskParams::Prompt { text } => {
            if text.trim().is_empty() {
                return Err(ResponseError {
                    code: tauqe_protocol::errors::INVALID_PARAMS.to_string(),
                    message: "Model ask requires a non-empty prompt or a discussion target"
                        .to_string(),
                    data: None,
                });
            }
            match focus_to_target(&state.discussion_focus().await) {
                Some((target, focused_items)) => {
                    match build_discussion_prompt(state, &target, &focused_items, &text).await {
                        Ok(prompt) => Ok((prompt, intent_for_target(&target))),
                        Err(err) => {
                            // The focused entity is gone (for example the plan
                            // was deleted elsewhere): drop the stale anchor and
                            // answer the raw prompt instead of failing the turn.
                            tracing::warn!(
                                "Discussion focus target could not be loaded ({}); clearing focus",
                                err.message
                            );
                            state.clear_discussion_focus().await;
                            Ok((text, TurnIntent::InteractiveRepl))
                        }
                    }
                }
                None => Ok((text, TurnIntent::InteractiveRepl)),
            }
        }
    }
}

/// Maps a discussion target onto the turn intent it establishes.
///
/// A plan discussion is strictly read-only: the model reasons about the plan
/// and never touches files. A review finding dialogue stays interactive,
/// because the developer may ask the model to apply the fix directly.
pub(super) fn intent_for_target(target: &DiscussionTarget) -> TurnIntent {
    match target {
        DiscussionTarget::Plan { .. } => TurnIntent::ReadonlyDiscussion,
        DiscussionTarget::Review { .. } => TurnIntent::InteractiveRepl,
    }
}

/// Maps an explicit discussion target onto the focus anchor it establishes.
fn focus_from_target(discussion: &DiscussionRequest) -> DiscussionFocus {
    DiscussionFocus::from_target(discussion.target.clone(), discussion.focused_items.clone())
}

/// Maps an active focus anchor onto the discussion target it addresses.
///
/// Returns `None` for the free (unfocused) mode, so a plain prompt stays as-is.
fn focus_to_target(focus: &DiscussionFocus) -> Option<(DiscussionTarget, Vec<String>)> {
    focus
        .target()
        .map(|target| (target.clone(), focus.focused_items().to_vec()))
}

/// Builds the discussion prompt for a typed target, loading the referenced plan
/// or review session from storage.
async fn build_discussion_prompt(
    state: &Arc<AppState>,
    target: &DiscussionTarget,
    focused_items: &[String],
    comment: &str,
) -> Result<String, ResponseError> {
    let repo_root = state.repo_root().await;
    match target {
        DiscussionTarget::Plan { id } => {
            let loaded = {
                let repo_root = repo_root.clone();
                let id = id.clone();
                offload(move || tauqe_core::plan::storage::PlanStorage::load_plan(&repo_root, &id))
                    .await
                    .map_err(internal_error)?
            };
            let plan = match loaded {
                Ok(Some(plan)) => plan,
                Ok(None) => return Err(not_found(format!("Plan '{}' not found", id))),
                Err(err) => return Err(internal_error(format!("{:#}", err))),
            };
            Ok(tauqe_core::plan::prompt::format_plan_discussion_prompt(
                &plan,
                focused_items,
                comment,
            ))
        }
        DiscussionTarget::Review { id } => {
            let loaded = {
                let repo_root = repo_root.clone();
                let id = id.clone();
                offload(move || {
                    tauqe_core::review::storage::ReviewStorage::load_review(&repo_root, &id)
                })
                .await
                .map_err(internal_error)?
            };
            let session = match loaded {
                Ok(Some(session)) => session,
                Ok(None) => return Err(not_found(format!("Review session '{}' not found", id))),
                Err(err) => return Err(internal_error(format!("{:#}", err))),
            };
            let focused: Vec<u32> = focused_items
                .iter()
                .filter_map(|raw| raw.trim().parse::<u32>().ok())
                .collect();
            Ok(
                tauqe_core::review::prompt::format_review_discussion_prompt_items(
                    &session, &focused, comment,
                ),
            )
        }
    }
}

fn not_found(message: String) -> ResponseError {
    ResponseError {
        code: tauqe_protocol::errors::NOT_FOUND.to_string(),
        message,
        data: None,
    }
}

fn internal_error(message: String) -> ResponseError {
    ResponseError {
        code: tauqe_protocol::errors::INTERNAL_ERROR.to_string(),
        message,
        data: None,
    }
}
