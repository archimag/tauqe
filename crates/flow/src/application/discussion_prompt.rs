//! Preparation of interactive and focused discussion prompts.
use crate::api::{Application, DiscussionFocus, DiscussionTarget, FocusUpdate};
use std::path::Path;

pub enum ModelAskInput {
    Prompt {
        text: String,
    },
    Discussion {
        target: DiscussionTarget,
        focused_items: Vec<String>,
        comment: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DiscussionPromptError {
    #[error("Model ask requires a non-empty prompt or a discussion target")]
    InvalidInput,
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Storage(String),
}

#[derive(Debug)]
pub struct PreparedModelAsk {
    pub prompt: String,
    pub focus: Option<DiscussionTarget>,
    pub focus_update: Option<FocusUpdate>,
}

impl Application {
    /// Resolves target content before replacing focus. A stale implicit focus
    /// falls back to the original text; an explicit missing target is refused.
    pub(crate) async fn prepare_model_ask(
        &self,
        repo_root: &Path,
        input: ModelAskInput,
    ) -> Result<PreparedModelAsk, DiscussionPromptError> {
        match input {
            ModelAskInput::Discussion {
                target,
                focused_items,
                comment,
            } => {
                let prompt =
                    build_discussion_prompt(repo_root, &target, &focused_items, &comment).await?;
                let update = self
                    .set_discussion_focus(DiscussionFocus::Focused {
                        target: target.clone(),
                        focused_items,
                    })
                    .await;
                Ok(PreparedModelAsk {
                    prompt,
                    focus: Some(target),
                    focus_update: Some(update),
                })
            }
            ModelAskInput::Prompt { text } => {
                if text.trim().is_empty() {
                    return Err(DiscussionPromptError::InvalidInput);
                }
                match self.discussion_focus().await {
                    DiscussionFocus::None => Ok(PreparedModelAsk {
                        prompt: text,
                        focus: None,
                        focus_update: None,
                    }),
                    DiscussionFocus::Focused {
                        target,
                        focused_items,
                    } => {
                        match build_discussion_prompt(repo_root, &target, &focused_items, &text)
                            .await
                        {
                            Ok(prompt) => Ok(PreparedModelAsk {
                                prompt,
                                focus: Some(target),
                                focus_update: None,
                            }),
                            Err(error) => {
                                tracing::warn!("Discussion focus target could not be loaded ({}); clearing focus", error);
                                let update = self.clear_discussion_focus().await;
                                Ok(PreparedModelAsk {
                                    prompt: text,
                                    focus: None,
                                    focus_update: Some(update),
                                })
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| format!("blocking task failed: {}", err))
}

async fn build_discussion_prompt(
    repo_root: &Path,
    target: &DiscussionTarget,
    focused_items: &[String],
    comment: &str,
) -> Result<String, DiscussionPromptError> {
    match target {
        DiscussionTarget::Plan { id } => {
            let loaded = {
                let repo_root = repo_root.to_path_buf();
                let id = id.clone();
                offload(move || tauqe_core::plan::storage::PlanStorage::load_plan(&repo_root, &id))
                    .await
                    .map_err(DiscussionPromptError::Storage)?
            };
            let plan = match loaded {
                Ok(Some(plan)) => plan,
                Ok(None) => {
                    return Err(DiscussionPromptError::NotFound(format!(
                        "Plan '{}' not found",
                        id
                    )))
                }
                Err(err) => return Err(DiscussionPromptError::Storage(format!("{:#}", err))),
            };
            Ok(tauqe_core::plan::prompt::format_plan_discussion_prompt(
                &plan,
                focused_items,
                comment,
            ))
        }
        DiscussionTarget::Review { id } => {
            let loaded = {
                let repo_root = repo_root.to_path_buf();
                let id = id.clone();
                offload(move || {
                    tauqe_core::review::storage::ReviewStorage::load_review(&repo_root, &id)
                })
                .await
                .map_err(DiscussionPromptError::Storage)?
            };
            let session = match loaded {
                Ok(Some(session)) => session,
                Ok(None) => {
                    return Err(DiscussionPromptError::NotFound(format!(
                        "Review session '{}' not found",
                        id
                    )))
                }
                Err(err) => return Err(DiscussionPromptError::Storage(format!("{:#}", err))),
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
        DiscussionTarget::ReviewGroup {
            session_id,
            group_id,
        } => {
            let loaded = {
                let repo_root = repo_root.to_path_buf();
                let id = session_id.clone();
                offload(move || {
                    tauqe_core::review::storage::ReviewStorage::load_review(&repo_root, &id)
                })
                .await
                .map_err(DiscussionPromptError::Storage)?
            };
            let session = match loaded {
                Ok(Some(session)) => session,
                Ok(None) => {
                    return Err(DiscussionPromptError::NotFound(format!(
                        "Review session '{}' not found",
                        session_id
                    )))
                }
                Err(err) => return Err(DiscussionPromptError::Storage(format!("{:#}", err))),
            };
            let group = match session
                .effective_groups()
                .into_iter()
                .find(|group| group.id == group_id.as_str())
            {
                Some(group) => group,
                None => {
                    return Err(DiscussionPromptError::NotFound(format!(
                        "Review group '{}' not found in session '{}'",
                        group_id, session_id
                    )))
                }
            };
            // A group dialogue speaks for the whole group, so it carries every
            // finding that manifests the cause instead of a selection of them.
            // Membership follows the effective group of a finding: a finding
            // that stores no group belongs to the implicit default group.
            let findings: Vec<tauqe_core::review::ReviewItem> = session
                .items
                .iter()
                .filter(|item| {
                    let owner = item
                        .group
                        .as_deref()
                        .unwrap_or(tauqe_core::review::DEFAULT_REVIEW_GROUP_ID);
                    owner == group_id.as_str()
                })
                .cloned()
                .collect();
            Ok(
                tauqe_core::review::prompt::format_review_group_discussion_prompt(
                    &session, &group, &findings, comment,
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests;
