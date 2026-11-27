//! Execute one review member through the shared leaf queue.

use super::member::{ReviewSequenceResult, ReviewSequenceStop, ReviewStepStop};
use super::{resolve_finding_frame, resolve_group_frame, MemberRun, ReviewMember};
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::documents::Documents;
use crate::application::PlanExecutionEnvironment;
use crate::workflows::step::attempts::{
    ImplementationRunConfig, LeafExecutor, LeafOperations, LeafOutcome,
};
use crate::workflows::step::queue::{failure_reason, run_queue, QueueStop};
use crate::workflows::step::{
    completion::{classify_step_attempt, step_answer, CoderAttempt},
    run::{
        PreparedSingleStep, SingleStepRequest, StepLoopReport, StepResources, StepRunPublication,
    },
};
use async_trait::async_trait;
use std::{path::Path, sync::Arc};
use tauqe_core::context::{ContextAccess, ContextManager};
use tauqe_core::execution::EventSink;
use tauqe_core::llm::step::turn_capability_expr;
use tauqe_core::models::ModelSelection;
use tauqe_core::plan::{PlanImplementation, PlanItem, PlanItemStatus};
use tauqe_core::providers::{format_session_id, StreamEvent, SESSION_PREFIX_REVIEW};
use tauqe_core::review::IssueStatus;
use tauqe_core::workflow::batch::SoftStopPolicy;
use tauqe_core::workflow::implementation::{
    ImplementationFrame, StepFailure, StepTurnRequest, StepTurnResult,
};
use tauqe_core::workflow::EditProtocolKind;
use tokio::sync::{mpsc, oneshot};

type PublicationBinding = (String, EventSink);
type FirstAttempt = (PreparedSingleStep, oneshot::Sender<CoderAttempt>);

pub(super) struct ReviewSequence<'a> {
    pub step: &'a StepResources,
    pub root: &'a Path,
    pub operation: &'a OperationGuard,
    pub execution: &'a PlanExecutionEnvironment,
    pub selection: &'a ModelSelection,
    pub observer: &'a Arc<dyn ApplicationObserver>,
    pub documents: &'a Documents,
    pub member: &'a ReviewMember,
    pub stream_tx: &'a mpsc::Sender<StreamEvent>,
}

impl ReviewSequence<'_> {
    pub(super) async fn run(self, run: MemberRun) -> ReviewSequenceResult {
        if let Err(reason) = prepare_repository(self.root).await {
            return ReviewSequenceResult {
                executed_steps: Vec::new(),
                stop: ReviewSequenceStop::Interrupted(ReviewStepStop::Preparation(reason)),
                usage: None,
            };
        }

        let MemberRun { work, steps } = run;
        let config = ImplementationRunConfig {
            models: self.execution.models().clone(),
            marker: None,
            selection: Some(self.selection.clone()),
            instructions: None,
            queue: steps,
        };
        let (first_tx, first_rx) = oneshot::channel();
        let scope = self.step.prepare_usage_scope();
        let operations = ReviewAttempts {
            sequence: &self,
            target: ReviewLeafTarget {
                documents: self.documents.clone(),
                review_id: self.member.review_id().to_owned(),
                address: match self.member {
                    ReviewMember::Group { group_id, .. } => {
                        ReviewLeafAddress::Group(group_id.clone())
                    }
                    ReviewMember::Finding { item_id, .. } => ReviewLeafAddress::Finding(*item_id),
                },
            },
            first_attempt: std::sync::Mutex::new(Some(first_tx)),
            binding: std::sync::Mutex::new(None),
            usage: scope.binding(),
        };
        let sequence = &self;
        let queue = async {
            let cancel_rx = sequence.operation.hard_cancellation();
            // Soft stop gates the outer queue, not the leaves of the active member.
            let report = run_queue(
                LeafExecutor {
                    operations: &operations,
                    config: &config,
                },
                sequence.stream_tx,
                &sequence.operation.stop_token(),
                SoftStopPolicy::Ignore,
            )
            .await;
            let stop = match report.stop {
                QueueStop::Completed | QueueStop::Stopped if cancel_rx.is_cancelled() => {
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
                    .map(|(_, receipt)| receipt)
                    .collect(),
                stop,
                usage: None,
            }
        };
        tokio::pin!(queue);
        let published_operations = &operations;
        tokio::select! {
            report = &mut queue => report,
            Ok((prepared, response)) = first_rx => {
                // InProgress is already saved. Keep the same queue future under
                // one publication and usage window through all leaves and retries.
                self.documents.publish_review_snapshot(self.member.review_id(), self.observer.as_ref()).await;
                self.step.publish_step_run(
                    StepRunPublication {
                        identity: prepared.identity().clone(),
                        plan_context: None,
                        work_context: Some(work),
                        session_label: Some(format_session_id(SESSION_PREFIX_REVIEW, self.member.review_id())),
                    },
                    self.observer.clone(),
                    scope,
                    |id, sink| async move {
                        *published_operations.binding.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((id.clone(), sink.clone()));
                        let result = published_operations.execute_prepared_attempt(prepared, &id, sink).await;
                        let _ = response.send(result);
                        let report = queue.await;
                        // The publication relay waits for every sink to close. Do
                        // not retain its binding past the end of the leaf queue.
                        published_operations.binding.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
                        report
                    },
                    ReviewSequenceResult::edit_completion,
                ).await.map(|(mut report, receipt)| {
                    report.usage = Some(receipt);
                    report
                }).unwrap_or_else(|reason| ReviewSequenceResult {
                    executed_steps: Vec::new(),
                    stop: ReviewSequenceStop::Interrupted(ReviewStepStop::Failed(reason)),
                    usage: None,
                })
            }
        }
    }
}

struct ReviewAttempts<'a, 'run> {
    sequence: &'a ReviewSequence<'run>,
    target: ReviewLeafTarget,
    first_attempt: std::sync::Mutex<Option<oneshot::Sender<FirstAttempt>>>,
    binding: std::sync::Mutex<Option<PublicationBinding>>,
    usage: tauqe_core::execution::usage::UsageBinding,
}

#[async_trait]
impl LeafOperations for ReviewAttempts<'_, '_> {
    async fn read(&self) -> Result<ImplementationFrame, StepFailure> {
        self.target.read().await
    }

    async fn task(
        &self,
        frame: &ImplementationFrame,
        leaf: &PlanItem,
    ) -> Result<String, StepFailure> {
        match &self.target.address {
            ReviewLeafAddress::Group(_) => {
                tauqe_core::plan::prompt::format_plan_step_execution_prompt(
                    &frame.plan,
                    &leaf.id,
                    EditProtocolKind::Structured,
                    None,
                )
                .map_err(StepFailure::infrastructure)
            }
            ReviewLeafAddress::Finding(item_id) => {
                let session = self
                    .target
                    .documents
                    .review(&self.target.review_id)
                    .await
                    .map_err(StepFailure::infrastructure)?
                    .ok_or_else(|| {
                        StepFailure::infrastructure(format!(
                            "Review session '{}' not found",
                            self.target.review_id
                        ))
                    })?;
                tauqe_core::review::prompt::format_review_step_execution_prompt(
                    &session,
                    *item_id,
                    EditProtocolKind::Structured,
                    None,
                )
                .map_err(StepFailure::infrastructure)
            }
        }
    }

    fn retry_task(&self, task: &str, answer: &str, error: &str) -> String {
        match &self.target.address {
            ReviewLeafAddress::Group(_) => tauqe_core::plan::prompt::format_plan_step_retry_prompt(
                task,
                answer,
                Some(error),
                EditProtocolKind::Structured,
                None,
            ),
            ReviewLeafAddress::Finding(_) => {
                let mut prompt = tauqe_core::review::prompt::format_review_step_retry_prompt(
                    answer,
                    Some(error),
                    EditProtocolKind::Structured,
                    None,
                );
                prompt.push_str("\n---\n\n");
                prompt.push_str(task);
                prompt
            }
        }
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

    async fn attempt(&self, request: StepTurnRequest) -> CoderAttempt {
        let first = self
            .first_attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(first) = first {
            let cancel = request.cancel_rx.clone();
            let prepared = match self.prepare_attempt(request).await {
                Ok(prepared) => prepared,
                Err(_) if cancel.is_cancelled() => {
                    return CoderAttempt {
                        settlement: StepTurnResult::Cancelled.into(),
                        answer: String::new(),
                    }
                }
                Err(reason) => {
                    return CoderAttempt {
                        settlement: StepTurnResult::Failed(StepFailure::infrastructure(reason))
                            .into(),
                        answer: String::new(),
                    }
                }
            };
            let (tx, rx) = oneshot::channel();
            if first.send((prepared, tx)).is_err() {
                return Self::publication_closed();
            }
            return rx.await.unwrap_or_else(|_| Self::publication_closed());
        }
        let binding = self
            .binding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some((operation_id, sink)) = binding else {
            return Self::publication_closed();
        };
        self.execute_attempt(request, &operation_id, sink).await
    }
}

impl ReviewAttempts<'_, '_> {
    fn publication_closed() -> CoderAttempt {
        CoderAttempt {
            settlement: StepTurnResult::Failed(StepFailure::infrastructure(
                "Review member publication closed before the attempt",
            ))
            .into(),
            answer: String::new(),
        }
    }

    async fn execute_attempt(
        &self,
        request: StepTurnRequest,
        operation_id: &str,
        sink: EventSink,
    ) -> CoderAttempt {
        let cancel = request.cancel_rx.clone();
        let mut report = match self.run_attempt(request, operation_id, sink).await {
            Ok(StepLoopReport::Turn(report)) => report,
            _ if cancel.is_cancelled() => {
                return CoderAttempt {
                    settlement: StepTurnResult::Cancelled.into(),
                    answer: String::new(),
                }
            }
            Ok(StepLoopReport::Failed(reason)) | Err(reason) => {
                return CoderAttempt {
                    settlement: StepTurnResult::Failed(StepFailure::infrastructure(reason)).into(),
                    answer: String::new(),
                };
            }
        };
        report.cancelled |= cancel.is_cancelled();
        let answer = step_answer(&report);
        let settlement = classify_step_attempt(&report);
        // The disk receipt stands even if a later leaf cannot complete the member.
        if let (Some(hash), Some(edit)) = (settlement.confirmed_commit(), &settlement.edit) {
            self.sequence.observer.publish(
                operation_id,
                ApplicationEvent::Commit {
                    hash: hash.to_owned(),
                    summary: answer.clone(),
                    changed_files: edit.changed_files.clone(),
                },
            );
        }
        CoderAttempt { settlement, answer }
    }

    async fn run_attempt(
        &self,
        request: StepTurnRequest,
        operation_id: &str,
        sink: EventSink,
    ) -> Result<StepLoopReport, String> {
        let prepared = self.prepare_attempt(request).await?;
        Ok(self
            .sequence
            .step
            .execute_observed_step(
                self.sequence.operation,
                operation_id,
                prepared,
                sink,
                self.sequence.observer.clone(),
            )
            .await)
    }

    async fn execute_prepared_attempt(
        &self,
        prepared: PreparedSingleStep,
        operation_id: &str,
        sink: EventSink,
    ) -> CoderAttempt {
        let report = self
            .sequence
            .step
            .execute_observed_step(
                self.sequence.operation,
                operation_id,
                prepared,
                sink,
                self.sequence.observer.clone(),
            )
            .await;
        self.classify_attempt(report, operation_id)
    }

    fn classify_attempt(&self, report: StepLoopReport, operation_id: &str) -> CoderAttempt {
        let report = match report {
            StepLoopReport::Turn(report) => report,
            StepLoopReport::Failed(reason) => {
                return CoderAttempt {
                    settlement: StepTurnResult::Failed(StepFailure::infrastructure(reason)).into(),
                    answer: String::new(),
                };
            }
        };
        let answer = step_answer(&report);
        let settlement = classify_step_attempt(&report);
        if let (Some(hash), Some(edit)) = (settlement.confirmed_commit(), &settlement.edit) {
            self.sequence.observer.publish(
                operation_id,
                ApplicationEvent::Commit {
                    hash: hash.to_owned(),
                    summary: answer.clone(),
                    changed_files: edit.changed_files.clone(),
                },
            );
        }
        CoderAttempt { settlement, answer }
    }

    async fn prepare_attempt(
        &self,
        request: StepTurnRequest,
    ) -> Result<PreparedSingleStep, String> {
        let sequence = self.sequence;
        let mut context = sequence.step.context.context_for_turn().await;
        let listener = sequence
            .step
            .context
            .activate_research_publication(
                sequence.operation,
                sequence.operation.publication_id(),
                sequence.observer.clone(),
                &context,
            )
            .ok_or("Could not bind review preload context publication")?;
        context.set_listener(Some(listener));
        let prepared = prepare_context(
            context,
            sequence.root,
            request.files,
            request.readonly_files,
        )
        .await;
        let (context, missing) = match prepared {
            Ok(value) => value,
            Err(error) => {
                sequence
                    .step
                    .context
                    .close_research_publication(sequence.operation.id());
                let _ = sequence
                    .step
                    .context
                    .activate_manual_publication(sequence.operation, sequence.observer.clone());
                return Err(error);
            }
        };
        let mut task = request.prompt;
        if !missing.is_empty() {
            task.push_str("\n\n## Declared Context Files Unavailable\nThe following optional preload paths were not found (JSON-encoded paths):\n");
            for path in missing {
                task.push_str(&format!("- {}\n", serde_json::json!(path)));
            }
            task.push_str("Continue with the available context. Use research to locate current files if needed; these declarations are preload hints, not execution prerequisites.\n");
        }
        let prepared = sequence
            .step
            .prepare_single_step_with_usage(
                sequence.operation,
                SingleStepRequest {
                    selection: Some(request.selection),
                    execution: sequence.execution.clone(),
                    reasoning_effort: request.reasoning_effort,
                    session_id: request.session_id,
                    capabilities: turn_capability_expr(),
                    task,
                    context,
                },
                self.usage.clone(),
            )
            .await;
        if prepared.is_err() {
            sequence
                .step
                .context
                .close_research_publication(sequence.operation.id());
            let _ = sequence
                .step
                .context
                .activate_manual_publication(sequence.operation, sequence.observer.clone());
        }
        prepared
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

pub(super) async fn prepare_context(
    mut context: ContextManager,
    root: &Path,
    files: Vec<String>,
    readonly_files: Vec<String>,
) -> Result<(ContextManager, Vec<String>), String> {
    let root = root.to_path_buf();
    tauqe_core::blocking::run_blocking_async(move || {
        if context.repo_root() != root {
            return Err("Review context belongs to a different repository".into());
        }
        let mut missing = Vec::new();
        for (path, access) in files
            .into_iter()
            .map(|path| (path, ContextAccess::Editable))
            .chain(
                readonly_files
                    .into_iter()
                    .map(|path| (path, ContextAccess::ReadOnly)),
            )
        {
            // Declarations save research rounds; they do not require the file
            // to survive unchanged since the plan was written.
            let item = context
                .add_auto_file_if_exists(&path, access)
                .map_err(|error| format!("Failed to preload context file '{path}': {error:#}"))?;
            if item.is_none() {
                missing.push(path);
            }
        }
        Ok((context, missing))
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

#[cfg(test)]
mod context_tests {
    use super::*;

    #[tokio::test]
    async fn missing_preloads_do_not_discard_available_review_context() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in ["target.rs", "reference.rs", "kept.rs", "deleted.rs"] {
            std::fs::write(root.join(path), "source").unwrap();
        }
        std::fs::remove_file(root.join("deleted.rs")).unwrap();
        let mut context = ContextManager::new(root.to_path_buf());
        context
            .add_auto_file("kept.rs", ContextAccess::ReadOnly)
            .unwrap();
        let (context, missing) = prepare_context(
            context,
            root,
            vec!["new.rs".into(), "target.rs".into()],
            vec![
                "deleted.rs".into(),
                "reference.rs".into(),
                "missing/test.rs".into(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(missing, ["new.rs", "deleted.rs", "missing/test.rs"]);
        let state = context.get_state();
        assert_eq!(state.items.len(), 3);
        assert_eq!(
            state
                .items
                .iter()
                .find(|item| item.path == "kept.rs")
                .unwrap()
                .layer,
            tauqe_core::context::ContextLayer::Auto
        );
        assert!(state
            .items
            .iter()
            .filter(|item| item.path != "kept.rs")
            .all(|item| item.layer == tauqe_core::context::ContextLayer::Auto));
        for (path, access) in [
            ("kept.rs", ContextAccess::ReadOnly),
            ("target.rs", ContextAccess::Editable),
            ("reference.rs", ContextAccess::ReadOnly),
        ] {
            assert!(state
                .items
                .iter()
                .any(|item| item.path == path && item.access == access));
        }
    }

    #[tokio::test]
    async fn review_preload_does_not_hide_invalid_paths_as_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("directory")).unwrap();
        for path in ["../missing.rs", "directory"] {
            let error = prepare_context(
                ContextManager::new(dir.path().to_path_buf()),
                dir.path(),
                Vec::new(),
                vec![path.into()],
            )
            .await
            .unwrap_err();
            assert!(error.contains("Failed to preload context file"), "{error}");
        }
    }
}
