//! Execute one review member through the shared leaf queue.

use super::member::{ReviewLeafReceipt, ReviewSequenceResult, ReviewSequenceStop, ReviewStepStop};
use super::{resolve_finding_frame, resolve_group_frame, MemberRun};
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::documents::Documents;
use crate::application::turn_setup::ResolvedTurnModel;
use crate::application::PlanExecutionEnvironment;
use crate::workflows::step::attempts::{
    ImplementationRunConfig, LeafExecutor, LeafOperations, LeafOutcome,
};
use crate::workflows::step::queue::{failure_reason, run_queue, QueueStop};
use crate::workflows::step::{
    completion::{classify_step_attempt, step_response},
    run::{SingleStepRequest, StepLoopReport, StepResources, StepRunPublication},
};
use async_trait::async_trait;
use std::{path::Path, sync::Arc};
use tauqe_core::context::{ContextAccess, ContextManager};
use tauqe_core::execution::EventSink;
use tauqe_core::llm::{step::turn_capability_expr, LlmCommand};
use tauqe_core::plan::{PlanImplementation, PlanItemStatus};
use tauqe_core::providers::{format_session_id, StreamEvent, SESSION_PREFIX_REVIEW};
use tauqe_core::review::IssueStatus;
use tauqe_core::workflow::implementation::{
    ExecutionScope, ImplementationFrame, StepFailure, StepTurnRequest, StepTurnResult,
    StepTurnSettlement,
};
use tauqe_core::workflow::EditProtocolKind;
use tokio::sync::{mpsc, watch, Notify};

type PublicationBinding = (String, EventSink);

pub(super) struct ReviewSequence<'a> {
    pub step: &'a StepResources,
    pub root: &'a Path,
    pub operation: &'a OperationGuard,
    pub execution: &'a PlanExecutionEnvironment,
    pub model: &'a ResolvedTurnModel,
    pub observer: &'a Arc<dyn ApplicationObserver>,
    pub documents: &'a Documents,
    pub scope: &'a ExecutionScope,
    pub review_id: &'a str,
    pub stream_tx: &'a mpsc::Sender<StreamEvent>,
}

impl ReviewSequence<'_> {
    pub(super) async fn run(self, run: MemberRun) -> ReviewSequenceResult {
        if let Err(reason) = prepare_repository(self.root).await {
            return ReviewSequenceResult {
                executed_steps: Vec::new(),
                stop: ReviewSequenceStop::Interrupted(ReviewStepStop::Preparation(reason)),
            };
        }

        let config = ImplementationRunConfig {
            models: self.execution.models().clone(),
            protocol: EditProtocolKind::Structured,
            marker: None,
            selection: Some(self.model.selection.clone()),
            instructions: None,
            queue: run.steps,
        };
        let ready = Notify::new();
        let (binding_tx, binding_rx) = watch::channel(None);
        let first_attempt = &ready;
        let sequence = &self;
        let queue = async move {
            let operations = ReviewAttempts {
                sequence,
                target: ReviewLeafTarget {
                    documents: sequence.documents.clone(),
                    review_id: sequence.review_id.to_owned(),
                    address: match sequence.scope {
                        ExecutionScope::ReviewGroup { group_id, .. } => {
                            ReviewLeafAddress::Group(group_id.clone())
                        }
                        ExecutionScope::ReviewFinding { item_id, .. } => {
                            ReviewLeafAddress::Finding(*item_id)
                        }
                        ExecutionScope::Plan { .. } => {
                            return ReviewSequenceResult {
                                executed_steps: Vec::new(),
                                stop: ReviewSequenceStop::Interrupted(ReviewStepStop::Failed(
                                    "Review run must address a review member".into(),
                                )),
                            };
                        }
                    },
                },
                first_attempt,
                binding_rx,
            };
            let cancel_rx = sequence.operation.cancel_receiver();
            // Soft stop gates the outer queue, not the leaves of the active member.
            let (_stop_tx, stop_rx) = watch::channel(false);
            let report = run_queue(
                LeafExecutor {
                    operations: &operations,
                    config: &config,
                },
                sequence.stream_tx,
                &cancel_rx,
                &stop_rx,
            )
            .await;
            let stop = match report.stop {
                QueueStop::Completed | QueueStop::Stopped if *cancel_rx.borrow() => {
                    ReviewSequenceStop::Interrupted(ReviewStepStop::Cancelled)
                }
                QueueStop::Completed => ReviewSequenceStop::Completed,
                QueueStop::Stopped => ReviewSequenceStop::Interrupted(ReviewStepStop::Failed(
                    "Review leaf queue stopped despite soft stop being disabled".into(),
                )),
                QueueStop::Cancelled => ReviewSequenceStop::Interrupted(ReviewStepStop::Cancelled),
                QueueStop::Blocked { reason, .. } => {
                    ReviewSequenceStop::Interrupted(ReviewStepStop::Blocked(reason))
                }
                QueueStop::Failed { outcome, .. } => {
                    let reason = failure_reason(&outcome);
                    ReviewSequenceStop::Interrupted(match *outcome {
                        LeafOutcome::Exhausted(..) => ReviewStepStop::Exhausted(reason),
                        LeafOutcome::TransitionFailed { .. } => ReviewStepStop::Storage(reason),
                        _ => ReviewStepStop::Failed(reason),
                    })
                }
            };
            ReviewSequenceResult {
                executed_steps: report
                    .completed
                    .into_iter()
                    .map(|(_, receipt)| ReviewLeafReceipt {
                        commit: receipt.confirmed_commit().map(str::to_owned),
                        modified_paths: receipt
                            .edit
                            .map(|edit| edit.changed_files)
                            .unwrap_or_default(),
                    })
                    .collect(),
                stop,
            }
        };
        tokio::pin!(queue);
        tokio::select! {
            report = &mut queue => report,
            () = ready.notified() => {
                // InProgress is already saved. Keep the same queue future under
                // one publication and usage window through all leaves and retries.
                self.documents.publish_review_snapshot(self.review_id, self.observer.as_ref()).await;
                self.step.publish_step_run(
                    StepRunPublication {
                        model: self.model.model.clone(),
                        role: self.model.role,
                        effort: self.model.effort,
                        alias: self.model.alias.clone(),
                        plan_context: None,
                        work_context: Some(run.work),
                        session_label: Some(format_session_id(SESSION_PREFIX_REVIEW, self.review_id)),
                    },
                    self.observer.clone(),
                    |id, sink| async move {
                        binding_tx.send_replace(Some((id, sink)));
                        drop(binding_tx);
                        queue.await
                    },
                    ReviewSequenceResult::edit_completion,
                ).await
            }
        }
    }
}

struct ReviewAttempts<'a, 'run> {
    sequence: &'a ReviewSequence<'run>,
    target: ReviewLeafTarget,
    first_attempt: &'a Notify,
    binding_rx: watch::Receiver<Option<PublicationBinding>>,
}

#[async_trait]
impl LeafOperations for ReviewAttempts<'_, '_> {
    async fn read(&self) -> Result<ImplementationFrame, StepFailure> {
        self.target.read().await
    }

    fn session_id(&self) -> String {
        self.target.session_id()
    }

    async fn begin(&self, step_id: &str) -> Result<Option<PlanImplementation>, StepFailure> {
        self.target.begin(step_id).await
    }

    async fn finish(&self, step_id: &str, outcome: &LeafOutcome) -> Result<(), StepFailure> {
        self.target.finish(step_id, outcome).await
    }

    async fn attempt(&self, request: StepTurnRequest) -> (StepTurnSettlement, String) {
        self.first_attempt.notify_one();
        let mut rx = self.binding_rx.clone();
        let binding = rx
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|value| value.clone());
        let Some((operation_id, sink)) = binding else {
            return (
                StepTurnResult::Failed(StepFailure::infrastructure(
                    "Review member publication closed before the attempt",
                ))
                .into(),
                String::new(),
            );
        };
        let cancel = request.cancel_rx.clone();
        let mut report = match self.run_attempt(request, &operation_id, sink).await {
            Ok(StepLoopReport::Turn(report)) => report,
            _ if *cancel.borrow() => return (StepTurnResult::Cancelled.into(), String::new()),
            Ok(StepLoopReport::Failed(reason)) | Err(reason) => {
                return (
                    StepTurnResult::Failed(StepFailure::infrastructure(reason)).into(),
                    String::new(),
                );
            }
        };
        report.cancelled |= *cancel.borrow();
        let answer = step_response(&report)
            .and_then(|response| {
                response
                    .commands()
                    .iter()
                    .rev()
                    .find_map(|command| match command {
                        LlmCommand::Text(message) => Some(message.content.clone()),
                        _ => None,
                    })
            })
            .unwrap_or_default();
        let settlement = classify_step_attempt(&report);
        // The disk receipt stands even if a later leaf cannot complete the member.
        if let (Some(hash), Some(edit)) = (settlement.confirmed_commit(), &settlement.edit) {
            self.sequence.observer.publish(
                &operation_id,
                ApplicationEvent::Commit {
                    hash: hash.to_owned(),
                    summary: answer.clone(),
                    changed_files: edit.changed_files.clone(),
                },
            );
        }
        (settlement, answer)
    }
}

impl ReviewAttempts<'_, '_> {
    async fn run_attempt(
        &self,
        request: StepTurnRequest,
        operation_id: &str,
        sink: EventSink,
    ) -> Result<StepLoopReport, String> {
        let sequence = self.sequence;
        let context = prepare_context(
            sequence.step.context.context_for_turn().await,
            sequence.root,
            request.files,
            request.readonly_files,
        )
        .await?;
        let prepared = sequence
            .step
            .prepare_single_step(
                sequence.operation,
                SingleStepRequest {
                    selection: Some(request.selection),
                    execution: sequence.execution.clone(),
                    reasoning_effort: request.reasoning_effort,
                    session_id: request.session_id,
                    capabilities: turn_capability_expr(),
                    task: request.prompt,
                    context,
                },
            )
            .await?;
        Ok(sequence
            .step
            .execute_prepared_single_step(sequence.operation, operation_id, prepared, sink)
            .await)
    }
}

async fn prepare_repository(root: &Path) -> Result<(), String> {
    let root = root.to_path_buf();
    tauqe_core::blocking::run_blocking_async(move || {
        tauqe_core::git::checkpoint::ensure_tauqe_ignored(&root)
    })
    .await
    .map_err(|error| format!("infrastructure error: {error}"))?
    .map_err(|error| format!("git error: {error}"))
}

async fn prepare_context(
    mut context: ContextManager,
    root: &Path,
    files: Vec<String>,
    readonly_files: Vec<String>,
) -> Result<ContextManager, String> {
    let root = root.to_path_buf();
    tauqe_core::blocking::run_blocking_async(move || {
        for path in files {
            if root.join(&path).is_file() {
                context
                    .add_file(&path, ContextAccess::Editable)
                    .map_err(|error| {
                        format!("Failed to add target file '{path}' to context: {error}")
                    })?;
            }
        }
        for path in readonly_files {
            context
                .add_file(&path, ContextAccess::ReadOnly)
                .map_err(|error| {
                    format!("Failed to add contract file '{path}' to context: {error}")
                })?;
        }
        Ok(context)
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(crate) enum ReviewLeafAddress {
    Group(String),
    Finding(u32),
}

pub(crate) struct ReviewLeafTarget {
    pub documents: Documents,
    pub review_id: String,
    pub address: ReviewLeafAddress,
}

impl ReviewLeafTarget {
    pub(crate) async fn read(&self) -> Result<ImplementationFrame, StepFailure> {
        match &self.address {
            ReviewLeafAddress::Group(group) => {
                resolve_group_frame(&self.documents, &self.review_id, group).await
            }
            ReviewLeafAddress::Finding(item) => {
                resolve_finding_frame(&self.documents, &self.review_id, *item).await
            }
        }
    }

    pub(crate) fn session_id(&self) -> String {
        format_session_id(SESSION_PREFIX_REVIEW, &self.review_id)
    }

    pub(crate) async fn begin(
        &self,
        step_id: &str,
    ) -> Result<Option<PlanImplementation>, StepFailure> {
        match &self.address {
            ReviewLeafAddress::Group(group) => self
                .documents
                .write_group_leaf(
                    &self.review_id,
                    group,
                    step_id,
                    PlanItemStatus::InProgress,
                    None,
                    None,
                )
                .await
                .map(Some),
            ReviewLeafAddress::Finding(item) => {
                self.documents
                    .write_finding_status(&self.review_id, *item, IssueStatus::InProgress)
                    .await?;
                Ok(None)
            }
        }
    }

    pub(crate) async fn finish(
        &self,
        step_id: &str,
        outcome: &LeafOutcome,
    ) -> Result<(), StepFailure> {
        let completion = match outcome {
            LeafOutcome::Completed(receipt) => {
                let StepTurnResult::Success { commit, .. } = &receipt.result else {
                    return Err(StepFailure::infrastructure(
                        "Completed leaf without a successful turn",
                    ));
                };
                Some(commit.as_deref())
            }
            LeafOutcome::Cancelled(Some(receipt))
                if receipt.result == StepTurnResult::Cancelled =>
            {
                receipt.confirmed_commit().map(Some)
            }
            _ => None,
        };
        let reset = completion.is_none() && !matches!(outcome, LeafOutcome::Blocked(..));
        let saved = match &self.address {
            ReviewLeafAddress::Group(group) => {
                let (status, blocker, commit) = match (completion, outcome) {
                    (Some(commit), _) => (PlanItemStatus::Done, None, commit),
                    (_, LeafOutcome::Blocked(reason, _)) => {
                        (PlanItemStatus::Blocked, Some(reason.as_str()), None)
                    }
                    _ => (PlanItemStatus::Todo, None, None),
                };
                self.documents
                    .write_group_leaf(&self.review_id, group, step_id, status, blocker, commit)
                    .await
                    .map(drop)
            }
            ReviewLeafAddress::Finding(item) => {
                // A leaf records a commit; only the member can close the finding.
                if let Some(commit) = completion
                    .flatten()
                    .map(str::trim)
                    .filter(|hash| !hash.is_empty())
                {
                    self.documents
                        .write_finding_commit(&self.review_id, *item, commit)
                        .await
                } else if reset {
                    self.documents
                        .write_finding_status(&self.review_id, *item, IssueStatus::Todo)
                        .await
                } else {
                    Ok(())
                }
            }
        };
        match saved {
            Err(failure) if reset && !matches!(outcome, LeafOutcome::Exhausted(..)) => {
                tracing::warn!(
                    step_id,
                    error = failure.reason(),
                    "failed to return an implementation step to TODO"
                );
                Ok(())
            }
            result => result,
        }
    }
}
