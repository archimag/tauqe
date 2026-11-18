//! RPC adaptation for core-owned model ask preparation.
use crate::contracts::Project;
use crate::state::AppState;
use std::sync::Arc;
use tauqe_core::api::{DiscussionPromptError, ModelAskInput};
use tauqe_protocol::{DiscussionTarget, ModelAskParams, ResponseError};

#[derive(Debug)]
pub(super) struct ModelAskTurn {
    pub(super) prompt: String,
    pub(super) focus: Option<DiscussionTarget>,
}

pub(super) async fn build_model_ask_prompt(
    state: &Arc<AppState>,
    params: ModelAskParams,
) -> Result<ModelAskTurn, ResponseError> {
    let input = match params {
        ModelAskParams::Prompt { text } => ModelAskInput::Prompt { text },
        ModelAskParams::Discussion(request) => ModelAskInput::Discussion {
            target: request.target.project(),
            focused_items: request.focused_items,
            comment: request.comment,
        },
    };
    let root = state.repo_root().await;
    let prepared = state
        .application
        .prepare_model_ask(&root, input)
        .await
        .map_err(|error| {
            let code = match &error {
                DiscussionPromptError::InvalidInput => tauqe_protocol::errors::INVALID_PARAMS,
                DiscussionPromptError::NotFound(_) => tauqe_protocol::errors::NOT_FOUND,
                DiscussionPromptError::Storage(_) => tauqe_protocol::errors::INTERNAL_ERROR,
            };
            ResponseError::new(code, error.to_string())
        })?;
    if let Some(update) = prepared.focus_update {
        state.publish_focus_update(update);
    }
    Ok(ModelAskTurn {
        prompt: prepared.prompt,
        focus: prepared.focus.map(Project::project),
    })
}
