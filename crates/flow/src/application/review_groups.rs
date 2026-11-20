//! Group review execution, finding persistence, and batch stop policy.

use crate::api::{Application, OperationGuard, ReviewObserver};
use crate::application::infrastructure::execution::PlanExecutionEnvironment;
use crate::application::infrastructure::structured_task::{self, StructuredTaskError};
use crate::application::plan_horizon::ArchitectScope;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::control::structured::StructuredError;
use tauqe_core::models::ModelSelection;
use tauqe_core::providers::{ChatMessage, StreamEvent};
use tauqe_core::review::parser::group_review_contract;
use tauqe_core::review::stream::ReviewStreamUpdate;
use tauqe_core::review::window::{expand_window, WindowBudgets, WindowContext};
use tauqe_core::review::{build_review_user_prompt, ReviewStorage};
use tauqe_core::review::{ReviewGroup, ReviewSession};
use tauqe_core::workflow::batch::{
    BatchExecution, BatchReport, BatchStep, BatchStepExecutor, BatchStepOutcome,
};
use tokio::sync::{mpsc, watch};

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

async fn offload_review<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| format!("blocking task failed: {error}"))?
        .map_err(|error| format!("{error:#}"))
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
    application: &'a Application,
    observer: Arc<dyn GroupAuditObserver>,
    exit_deadline: std::time::Duration,
    repo_root: &'a Path,
    /// Snapshot of the audited session: it supplies the model, the identifier
    /// bound to the provider session, and the groups themselves.
    session: &'a ReviewSession,
    /// Groups of that snapshot, matched by the step identifier.
    groups: &'a [ReviewGroup],
    environment: &'a PlanExecutionEnvironment,
    selection: &'a ModelSelection,
    budgets: WindowBudgets,
    /// Registered operation, so a stalled streamed call can be aborted by the
    /// hard-cancel watchdog instead of outliving its turn.
    operation: &'a Arc<OperationGuard>,
    /// Identifier of the registered operation, carried by every published event.
    operation_id: String,
    /// Next item ordinal, kept unique across groups.
    next_item_id: u32,
}

impl GroupAuditExecutor<'_> {
    /// Audits one group in isolation and appends its findings to the session.
    ///
    /// Returns the number of findings the group appended.
    async fn audit_group(
        &mut self,
        group: &ReviewGroup,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<usize, GroupFailure> {
        // The group audit expands its declared paths through the same shared
        // projection the plain launch and the read-only preview consume, so a
        // preset never applies its own selection order or its own limits
        // (#c-1, #c-5).
        let expanded = expand_window(
            self.repo_root,
            WindowContext::empty(),
            &group.paths,
            self.budgets,
        )
        .await
        .map_err(|error| GroupFailure::Infrastructure(error.to_string()))?;

        self.observer
            .group_started(group, expanded.files.len(), expanded.estimated_tokens);
        if expanded.window.omitted_by_budget > 0 {
            tracing::warn!(
                review_id = self.session.id.as_str(),
                group_id = group.id.as_str(),
                omitted = expanded.window.omitted_by_budget,
                "review group expansion exhausted the configured file token budget; remaining matched files were not loaded"
            );
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
                self.application,
                self.operation,
                self.selection,
                &ArchitectScope::Review(self.session.id.clone()),
            )
            .await
            .map_err(|error| GroupFailure::Infrastructure(error.to_string()))?;

        // The guard opens the accounting window before the provider call so the
        // group reports both the usage the provider returns and the duration the
        // harness measures itself.
        let publisher = Arc::clone(&self.observer);
        let usage_guard = self
            .application
            .begin_usage(&self.operation_id, move |id, snapshot| {
                publisher.publish_usage_totals(id, snapshot)
            })
            .await;

        // Each group owns a fresh request. The structured contract filters JSON
        // and decodes findings; only reasoning is shown during a group audit.
        let result = structured_task::run(
            self.application,
            self.operation,
            &execution,
            group_review_contract(),
            messages,
            &self.operation_id,
            self.exit_deadline,
            |event| {
                let observer = &self.observer;
                let usage = &usage_guard;
                let mut pacing_cancel = cancel_rx.clone();
                async move {
                    observer.pace(&event, &mut pacing_cancel).await;
                    match event {
                        StreamEvent::Usage(reported) => usage.observe(&reported).await,
                        StreamEvent::ReasoningDelta(delta) => {
                            observer.publish(ReviewStreamUpdate::Reasoning(delta))
                        }
                        _ => {}
                    }
                }
            },
        )
        .await;

        // Account for the tokens, cost, and duration the group consumed before
        // any early return, so a failed or cancelled group is still charged.
        // Every report was already folded into the window as it arrived, so the
        // flush below charges the whole group instead of its last report alone.
        usage_guard.flush().await;
        let usage_snapshot = self.application.usage_snapshot().await;
        // Both totals are read from the authoritative ledger: the operation scope
        // the flush just closed and the session scope it was folded into.
        self.observer.publish_usage(&usage_snapshot);

        if *cancel_rx.borrow() {
            return Err(GroupFailure::Cancelled);
        }
        let mut findings = match result {
            Ok(findings) => findings,
            Err(
                StructuredTaskError::Cancelled
                | StructuredTaskError::Request(StructuredError::Cancelled),
            ) => {
                return Err(GroupFailure::Cancelled);
            }
            Err(StructuredTaskError::Request(StructuredError::Empty)) => {
                return Err(GroupFailure::Domain(format!(
                    "Model returned an empty review for group '{}'",
                    group.title
                )));
            }
            Err(StructuredTaskError::Request(StructuredError::Decode(error))) => {
                return Err(GroupFailure::Domain(format!(
                    "Review group '{}' returned an unparsable payload: {error}",
                    group.title
                )));
            }
            Err(
                StructuredTaskError::Stream(error)
                | StructuredTaskError::Task(error)
                | StructuredTaskError::Request(StructuredError::Failed(error)),
            ) => {
                return Err(GroupFailure::Infrastructure(error));
            }
            Err(StructuredTaskError::Request(StructuredError::Incomplete)) => {
                return Err(GroupFailure::Infrastructure(
                    "Structured request closed without Done".into(),
                ));
            }
            Err(StructuredTaskError::ForeignOperation) => {
                return Err(GroupFailure::Infrastructure(
                    "group audit requires an active operation for this session".into(),
                ));
            }
            Err(StructuredTaskError::Orphaned) => {
                return Err(GroupFailure::Infrastructure(
                    "review group task did not stop within its exit deadline".into(),
                ));
            }
        };

        // Item identifiers are unique across the whole session: the parser
        // numbers the findings of one group from one, so the supervisor continues
        // the session's sequence instead of letting groups collide on `id`.
        let first_item_id = self.next_item_id;
        for (index, finding) in findings.iter_mut().enumerate() {
            finding.id = first_item_id.saturating_add(index as u32);
        }
        let appended = findings.len();

        let save_root = self.repo_root.to_path_buf();
        let review_id = self.session.id.clone();
        let group_id = group.id.clone();
        let stored = offload_review(move || {
            ReviewStorage::append_group_findings(&save_root, &review_id, &group_id, findings)
        })
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
        let list = self
            .application
            .documents_at(self.repo_root)
            .review_list()
            .await
            .ok();
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
        mut cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        // The slice is a shared reference with its own lifetime, so the lookup
        // result does not borrow the executor and `audit_group` can take `&mut self`.
        let groups = self.groups;
        let Some(group) = groups.iter().find(|group| group.id == step.id) else {
            return Ok(BatchStepOutcome::failed(format!(
                "Review group '{}' is not part of the audited session '{}'",
                step.id, self.session.id
            )));
        };

        let outcome = match self.audit_group(group, &mut cancel_rx).await {
            Ok(findings) => {
                self.observer.group_finished(group, findings, None);
                BatchStepOutcome::succeeded()
            }
            Err(GroupFailure::Domain(reason)) => {
                // A domain failure is confined to its own group: the terminal
                // event carries the diagnostic and the step reports success, so
                // the supervisor advances to the next group instead of
                // invalidating work the other groups can still complete.
                tracing::warn!(
                    review_id = self.session.id.as_str(),
                    group_id = group.id.as_str(),
                    reason = %reason,
                    "review group audit failed; the batch continues"
                );
                self.observer.group_finished(group, 0, Some(reason));
                BatchStepOutcome::succeeded()
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
        Ok(outcome)
    }
}

pub(crate) async fn run_group_audits(
    application: &Application,
    binding: (&PlanExecutionEnvironment, &ModelSelection),
    repo_root: &Path,
    session: &ReviewSession,
    operation: Arc<OperationGuard>,
    observer: Arc<dyn GroupAuditObserver>,
    exit_deadline: std::time::Duration,
) -> Result<BatchReport, String> {
    let (environment, selection) = binding;
    if !application.owns_operation(&operation) {
        return Err("group audit requires an active operation for this session".into());
    }
    let groups = session.effective_groups();
    let steps: Vec<BatchStep> = groups
        .iter()
        .map(|group| BatchStep::new(group.id.clone(), group.title.clone()))
        .collect();

    let budgets = WindowBudgets {
        files_tokens: environment.config().context.token_budget,
    };
    let mut executor = GroupAuditExecutor {
        application,
        observer,
        exit_deadline,
        repo_root,
        session,
        groups: &groups,
        environment,
        selection,
        budgets,
        operation: &operation,
        operation_id: operation.publication_id(),
        next_item_id: session
            .items
            .iter()
            .map(|item| item.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    };

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    // The batch borrows the caller's registered operation, so a cancellation
    // delivered between groups stops the queue and a soft stop drains it after the
    // group that is already running — the engine's supervisor semantics.
    let cancel_rx = operation.cancel_receiver();
    let soft_stop_rx = operation.soft_stop_receiver();

    let report = BatchExecution::new()
        .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
        .await;

    tracing::info!(
        review_id = session.id.as_str(),
        groups = steps.len(),
        executed = report.executed.len(),
        stop = ?report.stop,
        total_tokens = report.usage.total_tokens,
        "review group batch finished"
    );

    Ok(report)
}
