//! Explicit Architect synthesis followed by one group-owned write.

use super::{resolve_group_frame, ReviewResources};
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::configuration::Configuration;
use crate::application::documents::Documents;
use crate::application::PlanExecutionEnvironment;
use crate::workflows::architect::refinement::{
    synthesize_horizon, HorizonSynthesisRequest, VerdictKind, WaveFailure,
};
use crate::workflows::architect::ArchitectScope;
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::blocking::StartedOperations;
use tauqe_core::models::ModelSelection;
use tauqe_core::plan::{PlanItem, PlanItemStatus};
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::prompt::build_review_group_refine_user_prompt;
use tauqe_core::review::storage::GroupHorizonReplacementError;
use tauqe_core::review::{
    IssueStatus, ReviewGroup, ReviewGroupKind, ReviewItem, ReviewSession, DEFAULT_REVIEW_GROUP_ID,
};

#[cfg(test)]
pub(crate) mod test_gates {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};
    use tokio::sync::{Notify, Semaphore};

    pub(crate) struct Gate {
        pub entered: Notify,
        pub release: Semaphore,
    }

    impl Default for Gate {
        fn default() -> Self {
            Self {
                entered: Notify::new(),
                release: Semaphore::new(0),
            }
        }
    }

    impl Gate {
        pub async fn pause(&self) {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
    }

    #[derive(Default)]
    pub(crate) struct Gates {
        pub before_write: Gate,
        pub in_write: Gate,
        pub before_terminal: Gate,
    }

    fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Gates>>> {
        static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<Gates>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn for_root(root: &Path) -> Option<Arc<Gates>> {
        registry().lock().unwrap().get(root).cloned()
    }

    pub(crate) struct Installed(PathBuf);

    pub(crate) fn install(root: &Path, gates: Arc<Gates>) -> Installed {
        assert!(registry()
            .lock()
            .unwrap()
            .insert(root.into(), gates)
            .is_none());
        Installed(root.into())
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            registry().lock().unwrap().remove(&self.0);
        }
    }
}

pub struct GroupRefineInput {
    pub review_id: String,
    pub group_id: String,
    pub planner_model: Option<ModelSelection>,
    pub critic_model: Option<ModelSelection>,
    pub skip_critique: bool,
    pub instructions: Option<String>,
}

pub enum GroupRefineResult {
    Saved {
        group: Box<ReviewGroup>,
        explanation: String,
    },
    Blocked {
        reason: String,
    },
    Aborted {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum GroupRefineError {
    #[error("Another operation is already running")]
    Busy,
    #[error("review_id and group_id must be non-empty")]
    InvalidAddress,
    #[error("Review session or group not found")]
    NotFound,
    #[error("Only declared architectural groups can be refined")]
    NotArchitectural,
    #[error("The group has no open findings or unfinished steps")]
    NothingToRefine,
    #[error("Group refinement was cancelled")]
    Cancelled,
    #[error("The Architect returned an empty horizon")]
    EmptyHorizon,
    #[error("Invalid group horizon: {0}")]
    InvalidTree(String),
    #[error("The review group changed since refinement began: {0}")]
    StaleSnapshot(String),
    #[error("Architect synthesis failed: {0}")]
    Model(String),
    #[error("Group refinement storage failed: {0}")]
    Storage(String),
}

fn belongs_to(item: &ReviewItem, group_id: &str) -> bool {
    item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID) == group_id
}

fn unfinished(items: &[PlanItem]) -> bool {
    items.iter().any(|item| {
        if item.is_leaf() {
            item.status != PlanItemStatus::Done
        } else {
            unfinished(&item.children)
        }
    })
}

fn horizon_files(items: &[PlanItem], files: &mut Vec<String>) {
    for item in items {
        files.extend(item.files.iter().chain(&item.readonly_files).cloned());
        horizon_files(&item.children, files);
    }
}

fn prepare<'a>(
    session: &'a ReviewSession,
    group_id: &str,
) -> Result<(&'a ReviewGroup, Vec<ReviewItem>), GroupRefineError> {
    let group = session
        .groups
        .iter()
        .find(|group| group.id == group_id)
        .ok_or(GroupRefineError::NotFound)?;
    if group.kind != ReviewGroupKind::Architectural {
        return Err(GroupRefineError::NotArchitectural);
    }
    let findings: Vec<_> = session
        .items
        .iter()
        .filter(|item| belongs_to(item, group_id))
        .cloned()
        .collect();
    if !findings.iter().any(|item| {
        matches!(
            item.status,
            IssueStatus::Discussion | IssueStatus::Todo | IssueStatus::InProgress
        )
    }) && !unfinished(&group.implementation.items)
    {
        return Err(GroupRefineError::NothingToRefine);
    }
    Ok((group, findings))
}

fn wave_failure(failure: WaveFailure) -> Result<GroupRefineResult, GroupRefineError> {
    match failure {
        WaveFailure::Cancelled => Err(GroupRefineError::Cancelled),
        WaveFailure::Failed(reason) => Err(GroupRefineError::Model(reason)),
        WaveFailure::EmptyHorizon => Err(GroupRefineError::EmptyHorizon),
        WaveFailure::InvalidTree(reason) => Err(GroupRefineError::InvalidTree(reason)),
        WaveFailure::Verdict {
            kind: VerdictKind::Blocked,
            message,
        } => Ok(GroupRefineResult::Blocked { reason: message }),
        WaveFailure::Verdict {
            kind: VerdictKind::Aborted,
            message,
        } => Ok(GroupRefineResult::Aborted { reason: message }),
    }
}

fn write_failure(error: GroupHorizonReplacementError) -> GroupRefineError {
    match error {
        GroupHorizonReplacementError::StaleSnapshot(id) => GroupRefineError::StaleSnapshot(id),
        GroupHorizonReplacementError::EmptyHorizon => GroupRefineError::EmptyHorizon,
        GroupHorizonReplacementError::InvalidTree(error) => {
            GroupRefineError::InvalidTree(error.to_string())
        }
        GroupHorizonReplacementError::Storage(error) => {
            GroupRefineError::Storage(error.to_string())
        }
        other => GroupRefineError::Storage(other.to_string()),
    }
}

impl ReviewResources {
    pub(crate) async fn refine_group(
        &self,
        configuration: &Configuration,
        input: GroupRefineInput,
        observer: Arc<dyn ApplicationObserver>,
        exit_deadline: Duration,
    ) -> Result<GroupRefineResult, GroupRefineError> {
        let operation = self
            .step
            .execution
            .operations
            .begin()
            .map_err(|_| GroupRefineError::Busy)?;
        operation.reserve_supervisor_release();
        let step = crate::workflows::step::run::StepResources {
            context: self.step.context.clone(),
            execution: self.step.execution.clone(),
        };
        let snapshot = configuration.snapshot().await;
        tokio::spawn(async move {
            let bound = step
                .context
                .activate_manual_publication(&operation, observer.clone());
            let result = if bound {
                refine_owned(&step, snapshot, input, &operation, observer, exit_deadline).await
            } else {
                Err(GroupRefineError::Model(
                    "Could not bind group refinement context publication".into(),
                ))
            };
            if bound {
                if let Err(error) = step.context.clear_transient_context().await {
                    tracing::warn!(%error, "Failed to clear group refinement context");
                }
                step.context.close_manual_publication(&operation);
            }
            result
        })
        .await
        .map_err(|error| {
            GroupRefineError::Model(format!("Group refinement supervisor failed: {error}"))
        })?
    }
}

async fn refine_owned(
    step: &crate::workflows::step::run::StepResources,
    snapshot: crate::application::configuration::ConfigurationSnapshot,
    input: GroupRefineInput,
    operation: &OperationGuard,
    observer: Arc<dyn ApplicationObserver>,
    exit_deadline: Duration,
) -> Result<GroupRefineResult, GroupRefineError> {
    let GroupRefineInput {
        review_id,
        group_id,
        planner_model,
        critic_model,
        skip_critique,
        instructions,
    } = input;
    if review_id.trim().is_empty() || group_id.trim().is_empty() {
        return Err(GroupRefineError::InvalidAddress);
    }
    let root = step.context.repository_root().await;
    let documents = Documents::new(&root);
    let session = documents
        .review(&review_id)
        .await
        .map_err(GroupRefineError::Storage)?
        .ok_or(GroupRefineError::NotFound)?;
    let (group, findings) = prepare(&session, &group_id)?;
    let expected_group = group.clone();
    let plan = resolve_group_frame(&documents, &review_id, &group_id)
        .await
        .map_err(|error| GroupRefineError::Storage(error.into_reason()))?
        .plan;
    let mut files = Vec::new();
    horizon_files(&plan.implementation.items, &mut files);
    for item in &findings {
        files.extend(item.files.iter().chain(&item.readonly_files).cloned());
    }
    step.context
        .clear_transient_context()
        .await
        .map_err(|error| GroupRefineError::Storage(error.to_string()))?;
    step.context
        .preload_declared_context(root.clone(), Vec::new(), files)
        .await
        .map_err(|error| GroupRefineError::Storage(error.to_string()))?;
    if operation.hard_cancellation().is_cancelled() {
        return Err(GroupRefineError::Cancelled);
    }
    let environment = PlanExecutionEnvironment::new(snapshot, operation);
    let mut task = tauqe_core::plan::prompt::build_plan_horizon_system_prompt();
    task.push_str("\n\n---\n\n");
    task.push_str(&build_review_group_refine_user_prompt(
        &session,
        group,
        &findings,
        None,
        instructions.as_deref(),
    ));
    let horizon = match synthesize_horizon(
        &step.execution,
        &step.context,
        operation,
        &environment,
        observer.clone(),
        exit_deadline,
        HorizonSynthesisRequest {
            plan,
            draft_task: task,
            directives: instructions,
            scope: ArchitectScope::Review(review_id.clone()),
            planner_model,
            critic_model,
            skip_critique,
            operation_title: "Review Group Refinement",
        },
    )
    .await
    {
        Ok(horizon) => horizon,
        Err(failure) => return wave_failure(failure),
    };
    if operation.hard_cancellation().is_cancelled() {
        return Err(GroupRefineError::Cancelled);
    }
    #[cfg(test)]
    if let Some(gates) = test_gates::for_root(&root) {
        gates.before_write.pause().await;
        if operation.hard_cancellation().is_cancelled() {
            return Err(GroupRefineError::Cancelled);
        }
    }
    let operations = StartedOperations::default();
    let handle = documents
        .start_group_horizon_replacement(
            &operations,
            (review_id.clone(), group_id),
            (expected_group, findings),
            horizon.output.items,
            horizon.tier,
        )
        .map_err(|reason| GroupRefineError::Storage(reason.into()))?;
    operations.close();
    let summary = operations.wait().await;
    if summary.unconfirmed != 0 {
        return Err(GroupRefineError::Storage(
            "Group horizon write completion could not be confirmed".into(),
        ));
    }
    let group = handle
        .take_result()
        .await
        .ok_or_else(|| GroupRefineError::Storage("Group horizon write result was lost".into()))?
        .map_err(|error| GroupRefineError::Storage(error.to_string()))?
        .map_err(write_failure);
    let group = match group {
        Ok(group) => group,
        Err(error) => {
            observer.publish(
                &operation.publication_id(),
                ApplicationEvent::Stream(StreamEvent::Error(error.to_string())),
            );
            return Err(error);
        }
    };
    documents
        .publish_review_snapshot(&review_id, observer.as_ref())
        .await;
    #[cfg(test)]
    if let Some(gates) = test_gates::for_root(&root) {
        gates.before_terminal.pause().await;
    }
    horizon.terminal.publish(observer.as_ref());
    Ok(GroupRefineResult::Saved {
        group: Box::new(group),
        explanation: horizon.output.explanation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::review::{IssueType, ReviewIssue};

    #[test]
    fn admission_accepts_blocked_groups_and_refuses_closed_groups() {
        let mut group = ReviewGroup {
            id: "arch".into(),
            kind: ReviewGroupKind::Architectural,
            blocked_reason: Some("stalled".into()),
            ..Default::default()
        };
        let mut item = ReviewItem::from(ReviewIssue {
            id: 1,
            title: "Question".into(),
            uuid: None,
            commit: None,
            group: Some("arch".into()),
            model: None,
            issue_type: IssueType::Architecture,
            status: IssueStatus::Discussion,
            description: String::new(),
            recommendations: vec![],
            files: vec![],
            readonly_files: vec![],
            note: None,
        });
        let mut session = ReviewSession {
            groups: vec![group.clone()],
            items: vec![item.clone()],
            ..Default::default()
        };
        assert_eq!(prepare(&session, "arch").unwrap().1, vec![item.clone()]);
        item.status = IssueStatus::Fixed;
        session.items = vec![item];
        assert!(matches!(
            prepare(&session, "arch"),
            Err(GroupRefineError::NothingToRefine)
        ));
        group.implementation.items = vec![PlanItem {
            id: "1".into(),
            status: PlanItemStatus::Blocked,
            ..Default::default()
        }];
        session.groups = vec![group];
        assert!(prepare(&session, "arch").is_ok());
    }
}
