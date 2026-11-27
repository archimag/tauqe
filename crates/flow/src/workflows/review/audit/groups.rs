//! Group review execution, finding persistence, and batch stop policy.

use super::AuditResources;
use crate::api::OperationGuard;
use crate::application::documents::Documents;
use crate::application::review_observer::ReviewObserver;
use crate::application::usage_totals::UsageReceipt;
use crate::application::PlanExecutionEnvironment;
use crate::workflows::architect::ArchitectScope;
use crate::workflows::structured::{self, StructuredFailure};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::models::ModelSelection;
use tauqe_core::providers::{ChatMessage, StreamEvent};
use tauqe_core::review::build_review_user_prompt;
use tauqe_core::review::parser::group_review_contract;
use tauqe_core::review::stream::ReviewStreamUpdate;
use tauqe_core::review::window::expand_window;
use tauqe_core::review::{ReviewGroup, ReviewSession};
use tauqe_core::stop::HardCancellation;
use tauqe_core::workflow::batch::{
    BatchReport, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStepReport, BatchStepStatus,
    BatchStop, BatchUsage, QueueInterruption, SoftStopPolicy,
};
use tokio::sync::mpsc;

#[async_trait]
pub trait GroupAuditObserver: ReviewObserver {
    fn group_started(&self, group: &ReviewGroup, files: usize, tokens: usize);
    fn group_finished(&self, group: &ReviewGroup, findings: usize, error: Option<String>);
    async fn publish_review(
        &self,
        session: &ReviewSession,
        list: Option<(Vec<ReviewSession>, Option<String>)>,
    );
}

/// Why one group's audit did not produce findings.
///
/// The distinction is what keeps the batch isolated: a domain failure concerns
/// the group alone, while an infrastructure failure makes every later group
/// futile.
enum GroupFailure {
    /// The group produced no usable result; the queue continues.
    Domain(String),
    /// The environment could not deliver the audit; the queue stops.
    Infrastructure(String),
    /// The developer interrupted the run.
    Cancelled,
}

/// Executor that audits one review group per batch step.
struct GroupAuditExecutor<'a> {
    resources: &'a AuditResources,
    observer: Arc<dyn GroupAuditObserver>,
    exit_deadline: std::time::Duration,
    repo_root: &'a Path,
    documents: Documents,
    /// Snapshot of the audited session: it supplies the model, the identifier
    /// bound to the provider session, and the groups themselves.
    session: &'a ReviewSession,
    /// Groups of that snapshot, matched by the step identifier.
    groups: &'a [ReviewGroup],
    environment: &'a PlanExecutionEnvironment,
    selection: &'a ModelSelection,
    /// Registered operation, so a stalled streamed call can be aborted by the
    /// hard-cancel watchdog instead of outliving its turn.
    operation: &'a Arc<OperationGuard>,
    /// Identifier of the registered operation, carried by every published event.
    operation_id: String,
    /// Next item ordinal, kept unique across groups.
    next_item_id: u32,
    /// Receipt is retained even when decoding fails or cancellation wins.
    completed_receipt: Option<UsageReceipt>,
    last_receipt: Option<UsageReceipt>,
    domain_failure: bool,
}

impl GroupAuditExecutor<'_> {
    /// Audits one group in isolation and appends its findings to the session.
    ///
    /// Returns the number of findings the group appended.
    async fn audit_group(
        &mut self,
        group: &ReviewGroup,
        cancel_rx: &HardCancellation,
    ) -> Result<usize, GroupFailure> {
        // The group audit expands its declared paths through the same shared
        // projection the plain launch and the read-only preview consume, so a
        // preset never applies its own selection order or truncation rules
        // (#c-1, #c-5).
        let expanded = expand_window(self.repo_root, None, &group.paths)
            .await
            .map_err(|error| GroupFailure::Infrastructure(error.to_string()))?;

        self.observer
            .group_started(group, expanded.files.len(), expanded.estimated_tokens);
        if cancel_rx.is_cancelled() {
            return Err(GroupFailure::Cancelled);
        }
        if expanded.files.is_empty() {
            return Err(GroupFailure::Domain(format!(
                "Review group '{}' selected no repository files from paths: {}",
                group.title,
                group.paths.join(", ")
            )));
        }
        let files = expanded.files;

        let messages = vec![ChatMessage::user(build_review_user_prompt(
            &files,
            group.prompt.as_deref(),
        ))];
        let execution = self
            .environment
            .for_reviewer_structured(
                &self.resources.execution,
                self.operation,
                self.selection,
                &ArchitectScope::Review(self.session.id.clone()),
            )
            .await
            .map_err(|error| GroupFailure::Infrastructure(error.to_string()))?;

        // Each group owns a fresh request. The structured contract filters JSON
        // and decodes findings; only reasoning is shown during a group audit.
        let publisher = Arc::clone(&self.observer);
        let pacing_cancel = self.operation.hard_cancellation();
        let completed = structured::run_bound(
            &self.resources.execution,
            &self.resources.context,
            self.operation,
            &execution,
            group_review_contract(),
            messages,
            &self.operation_id,
            self.exit_deadline,
            move |id, snapshot| publisher.publish_usage_totals(id, snapshot),
            |event| {
                let observer = &self.observer;
                let pacing_cancel = pacing_cancel.clone();
                async move {
                    observer.pace(&event, &pacing_cancel).await;
                    if let StreamEvent::ReasoningDelta(delta) = event {
                        observer.publish(ReviewStreamUpdate::Reasoning(delta));
                    }
                }
            },
        )
        .await;

        let (result, receipt) = completed.map_err(|error| match error.classify("group audit") {
            StructuredFailure::Cancelled => GroupFailure::Cancelled,
            StructuredFailure::Failed(message) | StructuredFailure::Decode(message) => {
                GroupFailure::Infrastructure(message)
            }
            StructuredFailure::InputLimit(limit) => GroupFailure::Infrastructure(limit.to_string()),
            StructuredFailure::Empty => {
                GroupFailure::Infrastructure("Group audit supervisor returned no result".into())
            }
        })?;
        self.observer.publish_usage(&receipt.snapshot);
        self.completed_receipt = Some(receipt);

        if cancel_rx.is_cancelled() {
            return Err(GroupFailure::Cancelled);
        }
        let mut findings = match result {
            Ok(findings) => findings,
            Err(error) => match error.classify("group audit") {
                StructuredFailure::Cancelled => return Err(GroupFailure::Cancelled),
                StructuredFailure::Empty => {
                    return Err(GroupFailure::Domain(format!(
                        "Model returned an empty review for group '{}'",
                        group.title
                    )));
                }
                StructuredFailure::Decode(error) => {
                    return Err(GroupFailure::Domain(format!(
                        "Review group '{}' returned an unparsable payload: {error}",
                        group.title
                    )));
                }
                StructuredFailure::Failed(error) => {
                    return Err(GroupFailure::Infrastructure(error))
                }
                StructuredFailure::InputLimit(error) => {
                    return Err(GroupFailure::Infrastructure(error.to_string()))
                }
            },
        };

        // Item identifiers are unique across the whole session: the parser
        // numbers the findings of one group from one, so the supervisor continues
        // the session's sequence instead of letting groups collide on `id`.
        let first_item_id = self.next_item_id;
        for (index, finding) in findings.iter_mut().enumerate() {
            finding.id = first_item_id.saturating_add(index as u32);
        }
        let appended = findings.len();

        let stored = self
            .documents
            .append_review_group_findings(&self.session.id, &group.id, findings)
            .await;
        let session = match stored {
            Ok(Some(session)) => session,
            Ok(None) => {
                return Err(GroupFailure::Infrastructure(format!(
                    "Review session '{}' disappeared before group '{}' findings were appended",
                    self.session.id, group.title
                )))
            }
            Err(err) => return Err(GroupFailure::Infrastructure(err)),
        };
        self.next_item_id = first_item_id.saturating_add(appended as u32);

        // The appended findings are the group's durable result: publishing the
        // session and the refreshed list makes them visible immediately, so a
        // cancelled or failed run still shows every completed group.
        let list = self.documents.review_list().await.ok();
        self.observer.publish_review(&session, list).await;

        Ok(appended)
    }
}

#[async_trait]
impl BatchStepExecutor for GroupAuditExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        _stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: HardCancellation,
    ) -> anyhow::Result<BatchStepOutcome> {
        self.completed_receipt = None;
        self.domain_failure = false;
        // The slice is a shared reference with its own lifetime, so the lookup
        // result does not borrow the executor and `audit_group` can take `&mut self`.
        let groups = self.groups;
        let Some(group) = groups.iter().find(|group| group.id == step.id) else {
            return Ok(BatchStepOutcome::failed(format!(
                "Review group '{}' is not part of the audited session '{}'",
                step.id, self.session.id
            )));
        };

        let outcome = match self.audit_group(group, &cancel_rx).await {
            Ok(findings) => {
                self.observer.group_finished(group, findings, None);
                BatchStepOutcome::succeeded()
            }
            Err(GroupFailure::Domain(reason)) => {
                self.domain_failure = true;
                // The local failure is real even though independent groups can
                // still run. Continuation is decided by the audit queue below.
                tracing::warn!(
                    review_id = self.session.id.as_str(),
                    group_id = group.id.as_str(),
                    reason = %reason,
                    "review group audit failed; the batch continues"
                );
                self.observer.group_finished(group, 0, Some(reason.clone()));
                BatchStepOutcome::failed(reason)
            }
            Err(GroupFailure::Infrastructure(reason)) => {
                tracing::error!(
                    review_id = self.session.id.as_str(),
                    group_id = group.id.as_str(),
                    reason = %reason,
                    "review infrastructure failure stopped the group batch"
                );
                self.observer.group_finished(group, 0, Some(reason.clone()));
                BatchStepOutcome::failed(reason)
            }
            Err(GroupFailure::Cancelled) => {
                self.observer.group_finished(
                    group,
                    0,
                    Some(format!(
                        "Review group '{}' audit was cancelled",
                        group.title
                    )),
                );
                BatchStepOutcome::cancelled()
            }
        };
        Ok(match self.completed_receipt.take() {
            Some(receipt) => {
                let usage = receipt.batch_usage();
                self.last_receipt = Some(receipt);
                outcome.with_usage(usage)
            }
            None => outcome,
        })
    }
}

pub(super) async fn run_group_audits(
    resources: &AuditResources,
    binding: (&PlanExecutionEnvironment, &ModelSelection),
    repo_root: &Path,
    session: &ReviewSession,
    operation: Arc<OperationGuard>,
    observer: Arc<dyn GroupAuditObserver>,
    exit_deadline: std::time::Duration,
) -> Result<(BatchReport, Option<UsageReceipt>), String> {
    let (environment, selection) = binding;
    if !resources.execution.owns(&operation) {
        return Err("group audit requires an active operation for this session".into());
    }
    let groups = session.effective_groups();
    let steps: Vec<BatchStep> = groups
        .iter()
        .map(|group| BatchStep::new(group.id.clone(), group.title.clone()))
        .collect();

    let mut executor = GroupAuditExecutor {
        resources,
        observer,
        exit_deadline,
        repo_root,
        documents: Documents::new(repo_root),
        session,
        groups: &groups,
        environment,
        selection,
        operation: &operation,
        operation_id: operation.publication_id(),
        next_item_id: session
            .items
            .iter()
            .map(|item| item.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
        completed_receipt: None,
        last_receipt: None,
        domain_failure: false,
    };

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    // Unlike a fail-fast implementation batch, a review audit continues after
    // a local group failure. Keep the outcome in the report rather than
    // converting it into success to obtain that continuation.
    let cancel_rx = operation.hard_cancellation();
    let stop_token = operation.stop_token();
    let mut report = BatchReport {
        executed: Vec::with_capacity(steps.len()),
        scheduled: steps.len(),
        stop: BatchStop::Completed,
        usage: BatchUsage::default(),
    };
    for step in &steps {
        if let Some(interruption) =
            QueueInterruption::before_next(stop_token.state(), SoftStopPolicy::Observe)
        {
            report.stop = match interruption {
                QueueInterruption::Cancelled => BatchStop::Cancelled,
                QueueInterruption::Stopped => BatchStop::Stopped,
            };
            break;
        }
        let outcome = executor
            .execute_step(step, stream_tx.clone(), cancel_rx.clone())
            .await
            .unwrap_or_else(|error| BatchStepOutcome::failed(error.to_string()));
        report.usage.accumulate_outcome(&outcome);
        let status = outcome.status;
        report.executed.push(BatchStepReport {
            id: step.id.clone(),
            outcome,
        });
        match status {
            BatchStepStatus::Succeeded => {}
            BatchStepStatus::Failed if executor.domain_failure => {}
            BatchStepStatus::Cancelled => {
                report.stop = BatchStop::Cancelled;
                break;
            }
            _ => {
                report.stop = BatchStop::Failed {
                    step_id: step.id.clone(),
                };
                break;
            }
        }
    }

    tracing::info!(
        review_id = session.id.as_str(),
        groups = steps.len(),
        executed = report.executed.len(),
        stop = ?report.stop,
        total_tokens = report.usage.total_tokens,
        "review group batch finished"
    );

    Ok((report, executor.last_receipt))
}
