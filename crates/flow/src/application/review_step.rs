//! Domain lifecycle of one review horizon leaf.

use crate::api::{Application, OperationGuard};
use crate::application::infrastructure::execution::PlanExecutionEnvironment;
use crate::application::review_execution::{PlanStepArtifact, StepPersistence};
use std::path::Path;
use std::sync::Arc;
use tauqe_core::context::{ContextAccess, ContextManager};
use tauqe_core::control::context::{CancellationToken, EventSink};
use tauqe_core::control::outcome::{
    FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError,
};
use tauqe_core::control::turn::turn_capability_expr;
use tauqe_core::llm::{LlmCapability, LlmCommand};
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::plan::prompt::{format_plan_step_execution_prompt, format_plan_step_retry_prompt};
use tauqe_core::plan::{Plan, PlanItemStatus};
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::EditProtocolKind;

/// Execute a leaf with a new isolated technical unit on each coder attempt.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    app: &Application,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    root: &Path,
    context: &mut ContextManager,
    persistence: &Arc<dyn StepPersistence>,
    step_id: &str,
    initial_selection: Option<ModelSelection>,
    session_id: Option<String>,
    marker: Option<&str>,
    cancel: &CancellationToken,
    sink: &EventSink,
) -> Result<LoopResult<PlanStepArtifact>, SystemError> {
    if *operation.cancel_receiver().borrow() {
        return Err(SystemError::Cancelled);
    }
    let root_owned = root.to_path_buf();
    tauqe_core::blocking::run_blocking_async(move || {
        tauqe_core::git::checkpoint::ensure_tauqe_ignored(&root_owned)
    })
    .await
    .map_err(|e| SystemError::Other(e.to_string()))?
    .map_err(|e| SystemError::Git(e.to_string()))?;
    let mut plan = load(persistence, root).await?;
    let leaf = plan.implementation.find_item(step_id).ok_or_else(|| {
        SystemError::Other(format!(
            "Step '{}' is not part of plan '{}'",
            step_id, plan.id
        ))
    })?;
    if !leaf.is_leaf() {
        return Err(SystemError::Other(format!("Step '{}' ({}) is a container of the implementation horizon and never an execution target", leaf.id, leaf.title)));
    }
    match leaf.status {
        PlanItemStatus::Done => {
            return Ok(LoopResult::Success(LoopSuccess::new(
                PlanStepArtifact {
                    step_id: leaf.id.clone(),
                    commit: leaf.commit.clone(),
                    modified_paths: Vec::new(),
                },
                ResponseEnvelope::default(),
            )))
        }
        PlanItemStatus::Discussion | PlanItemStatus::Blocked | PlanItemStatus::Cancelled => {
            return Err(SystemError::Other(format!(
                "Step '{}' ({}) is {:?} and cannot be executed",
                leaf.id, leaf.title, leaf.status
            )));
        }
        PlanItemStatus::Todo | PlanItemStatus::InProgress => {}
    }
    let files = leaf.files.clone();
    let readonly = leaf.readonly_files.clone();
    let mut prepared = context.clone();
    let root_for_context = root.to_path_buf();
    let prepared = tauqe_core::blocking::run_blocking_async(move || {
        for path in files {
            if root_for_context.join(&path).is_file() {
                prepared
                    .add_file(&path, ContextAccess::Editable)
                    .map_err(|e| {
                        SystemError::Other(format!(
                            "Failed to add target file '{path}' to context: {e}"
                        ))
                    })?;
            }
        }
        for path in readonly {
            prepared
                .add_file(&path, ContextAccess::ReadOnly)
                .map_err(|e| {
                    SystemError::Other(format!(
                        "Failed to add contract file '{path}' to context: {e}"
                    ))
                })?;
        }
        Ok::<_, SystemError>(prepared)
    })
    .await
    .map_err(|e| SystemError::Other(e.to_string()))??;
    *context = prepared;
    let base =
        format_plan_step_execution_prompt(&plan, step_id, EditProtocolKind::Structured, marker)
            .map_err(SystemError::Other)?;
    let mut prompt = base.clone();
    transition(
        persistence,
        root,
        &mut plan,
        step_id,
        PlanItemStatus::InProgress,
        None,
        None,
    )
    .await?;

    let models = environment.models();
    let mut selection = initial_selection.unwrap_or(ModelSelection::Role(ModelRole::Coder));
    let mut rung = models.coder_tier_index(Some(&selection));
    let mut attempts = 0;
    let mut budget = models.resolve_selection_attempts(ModelRole::Coder, &selection);
    let mut model_name = models.resolve_model(&selection).to_string();
    loop {
        if *operation.cancel_receiver().borrow() {
            return Err(SystemError::Cancelled);
        }
        let effort = models.resolve_selection_effort(ModelRole::Coder, &selection);
        let mut execution = environment
            .for_edit_unit(
                app,
                operation,
                Some(selection.clone()),
                effort,
                session_id.clone(),
                turn_capability_expr(),
            )
            .await
            .map_err(|e| SystemError::Other(e.to_string()))?;
        let outcome = execution
            .run(root, context, None, cancel, sink, prompt.clone())
            .await;
        let (trigger, answer) = match outcome {
            Err(SystemError::RoundBudgetExhausted { .. }) => {
                ("Turn round budget exhausted".to_string(), String::new())
            }
            Err(error) => return Err(error),
            Ok(LoopResult::Success(success)) => {
                let artifact = success.value;
                if let Some(error) = artifact.commit_error {
                    transition(
                        persistence,
                        root,
                        &mut plan,
                        step_id,
                        PlanItemStatus::Todo,
                        None,
                        None,
                    )
                    .await?;
                    return Err(SystemError::Git(format!(
                        "Failed to commit the change set of step '{step_id}': {error}"
                    )));
                }
                if let Some(commit) = artifact.commit_hash {
                    transition(
                        persistence,
                        root,
                        &mut plan,
                        step_id,
                        PlanItemStatus::Done,
                        None,
                        Some(&commit),
                    )
                    .await?;
                    return Ok(LoopResult::Success(LoopSuccess::new(
                        PlanStepArtifact {
                            step_id: step_id.to_string(),
                            commit: Some(commit),
                            modified_paths: artifact.modified_paths,
                        },
                        success.response,
                    )));
                }
                if let Some((reason, details)) = blocked(&success.response) {
                    return block(
                        persistence,
                        root,
                        &mut plan,
                        step_id,
                        success.response,
                        &reason,
                        &details,
                    )
                    .await;
                }
                let answer = text(&success.response);
                let trigger = match success.response.commands().iter().find(|command| matches!(command,
                    LlmCommand::CommitMessage(_) | LlmCommand::TurnSkip(_) | LlmCommand::TurnError(_))) {
                    Some(LlmCommand::CommitMessage(_)) | Some(LlmCommand::TurnSkip(_)) => {
                        transition(persistence, root, &mut plan, step_id, PlanItemStatus::Done, None, None).await?;
                        return Ok(LoopResult::Success(LoopSuccess::new(PlanStepArtifact {
                            step_id: step_id.to_string(), commit: None, modified_paths: artifact.modified_paths,
                        }, success.response)));
                    }
                    Some(LlmCommand::TurnError(error)) if !error.error_message().trim().is_empty() => error.error_message().to_string(),
                    Some(LlmCommand::TurnError(_)) => "The step turn reported an error without stating a cause".into(),
                    _ => "The step turn produced neither a verified change set nor a declared outcome".into(),
                };
                (trigger, answer)
            }
            Ok(LoopResult::Diverged(response)) => {
                if let Some((reason, details)) = blocked(&response) {
                    return block(
                        persistence,
                        root,
                        &mut plan,
                        step_id,
                        response,
                        &reason,
                        &details,
                    )
                    .await;
                }
                (
                    "The step turn diverged without a completed change set".into(),
                    text(&response),
                )
            }
            Ok(LoopResult::Exhausted(report)) => (report.reason, String::new()),
        };
        attempts += 1;
        if attempts >= budget {
            if let Some((next, tier)) = models
                .auto_level_up
                .then(|| models.next_coder_tier(rung))
                .flatten()
            {
                sink.emit(StreamEvent::TierEscalated {
                    tier: next,
                    model: tier.model.clone(),
                    role: ModelRole::Coder,
                    effort: tier.effort,
                });
                rung = next;
                model_name = tier.model.to_string();
                budget = tier.attempts();
                selection = ModelSelection::Tier {
                    model: tier.model,
                    effort: tier.effort,
                };
                attempts = 0;
            } else {
                transition(
                    persistence,
                    root,
                    &mut plan,
                    step_id,
                    PlanItemStatus::Todo,
                    None,
                    None,
                )
                .await?;
                return Ok(LoopResult::Exhausted(FailureReport::new(
                    format!("{trigger}: escalation is exhausted on {model_name}"),
                    attempts as usize,
                    Vec::new(),
                )));
            }
        }
        prompt = format_plan_step_retry_prompt(
            &base,
            &answer,
            Some(&trigger),
            EditProtocolKind::Structured,
            marker,
        );
    }
}

fn text(response: &ResponseEnvelope) -> String {
    response
        .commands()
        .iter()
        .filter_map(|command| match command {
            LlmCommand::Text(message) => Some(message.content.as_str()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
        .to_string()
}

fn blocked(response: &ResponseEnvelope) -> Option<(String, String)> {
    response
        .commands()
        .iter()
        .find_map(|command| match command {
            LlmCommand::Blocked(block) => {
                Some((block.reason().to_string(), block.details().to_string()))
            }
            _ => None,
        })
}

async fn block(
    persistence: &Arc<dyn StepPersistence>,
    root: &Path,
    plan: &mut Plan,
    step_id: &str,
    mut response: ResponseEnvelope,
    reason: &str,
    details: &str,
) -> Result<LoopResult<PlanStepArtifact>, SystemError> {
    let combined = if details.trim().is_empty() {
        reason.trim().to_string()
    } else {
        format!("{}\n\n{}", reason.trim(), details.trim())
    };
    transition(
        persistence,
        root,
        plan,
        step_id,
        PlanItemStatus::Blocked,
        Some(&combined),
        None,
    )
    .await?;
    response.take(LlmCapability::Blocked);
    response.take_any(&[
        LlmCapability::CommitMessage,
        LlmCapability::TurnError,
        LlmCapability::TurnSkip,
    ]);
    Ok(LoopResult::Diverged(response))
}

async fn load(persistence: &Arc<dyn StepPersistence>, root: &Path) -> Result<Plan, SystemError> {
    let persistence = Arc::clone(persistence);
    let root = root.to_path_buf();
    tauqe_core::blocking::run_blocking_async(move || persistence.load(&root))
        .await
        .map_err(|e| SystemError::Other(e.to_string()))?
}

async fn transition(
    persistence: &Arc<dyn StepPersistence>,
    root: &Path,
    plan: &mut Plan,
    step_id: &str,
    status: PlanItemStatus,
    reason: Option<&str>,
    commit: Option<&str>,
) -> Result<(), SystemError> {
    fn find_leaf<'a>(
        items: &'a mut [tauqe_core::plan::PlanItem],
        id: &str,
    ) -> Option<&'a mut tauqe_core::plan::PlanItem> {
        for item in items {
            if item.id.trim().eq_ignore_ascii_case(id.trim()) {
                return item.is_leaf().then_some(item);
            }
            if let Some(found) = find_leaf(&mut item.children, id) {
                return Some(found);
            }
        }
        None
    }
    let item = find_leaf(&mut plan.implementation.items, step_id).ok_or_else(|| {
        SystemError::Other(format!("Step '{step_id}' is not part of the executed plan"))
    })?;
    item.status = status;
    if let Some(commit) = commit.map(str::trim).filter(|hash| !hash.is_empty()) {
        item.commit = Some(commit.to_string());
    }
    if status != PlanItemStatus::Done {
        item.commit = None;
    }
    item.blocked_reason = if status == PlanItemStatus::Blocked {
        reason
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    } else {
        None
    };
    plan.implementation.refresh_parent_statuses();
    plan.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let persistence = Arc::clone(persistence);
    let root = root.to_path_buf();
    let plan = plan.clone();
    tauqe_core::blocking::run_blocking_async(move || persistence.persist(&root, &plan))
        .await
        .map_err(|e| SystemError::Other(e.to_string()))?
}
