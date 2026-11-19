//! Group review execution, finding persistence, and batch stop policy.

use crate::api::{Application, OperationGuard, ReviewObserver};
use crate::application::provider_task::ProviderRequest;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::edits::stream::json::JsonFieldStreamFilter;
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::providers::{
    format_session_id, ChatMessage, LlmProvider, StreamEvent, SESSION_PREFIX_REVIEW,
};
use tauqe_core::review::parser::ReviewFindingsOutput;
use tauqe_core::review::stream::ReviewStreamUpdate;
use tauqe_core::review::window::{expand_window, WindowBudgets, WindowContext};
use tauqe_core::review::{
    build_review_system_prompt, build_review_user_prompt, parse_review_output,
    review_findings_response_format, ReviewStorage,
};
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
    /// Provider every group streams through.
    provider: Arc<dyn LlmProvider>,
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

        let messages = vec![
            ChatMessage::system(build_review_system_prompt()),
            ChatMessage::user(build_review_user_prompt(&files, group.prompt.as_deref())),
        ];
        // A group audit is a non-code operation: the enforced JSON schema turns
        // the reviewer output into machine-readable findings.
        let response_format = review_findings_response_format();
        // Every group of one audit extends the same provider prefix cache, so the
        // session identifier is derived from the review, not from the group.
        let session_id = format_session_id(SESSION_PREFIX_REVIEW, &self.session.id);
        let model_name = self.session.model.name.clone();
        let reasoning_effort = {
            let config = self.application.configuration().await.config.clone();
            config.models.resolve_selection_effort(
                ModelRole::Reviewer,
                &ModelSelection::Specific(self.session.model.clone()),
            )
        };

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

        let mut stream_task = self
            .application
            .start_provider_task(
                self.operation,
                ProviderRequest {
                    model_name,
                    messages,
                    response_format,
                    effort: reasoning_effort,
                    session_id,
                },
                Arc::clone(&self.provider),
                "review-group-stream",
            )
            .map_err(|error| GroupFailure::Infrastructure(error.to_string()))?;

        // The provider streams raw Structured Output JSON; the filter decodes the
        // `summary` string so the accumulated body keeps its contract shape while
        // JSON syntax never has to be inspected by hand.
        let mut filter = JsonFieldStreamFilter::<ReviewFindingsOutput>::new("summary");
        let mut failure: Option<String> = None;
        let mut stream_cancelled = false;

        // The group audit applies the transport-owned pacing policy instead of
        // restating an exemption set of its own: a terminal notice publishes
        // immediately, and every other variant waits for the writer to catch up,
        // so a client that keeps its pipe open but stopped reading pauses the
        // audit here instead of growing the outbound queue for the rest of the
        // batch.
        while let Some(event) = stream_task.next_event().await {
            self.observer.pace(&event, cancel_rx).await;
            match event {
                StreamEvent::TextDelta(chunk) => {
                    // A group audit renders no live text, so the decoded delta is
                    // discarded while the filter keeps accumulating the payload.
                    let _ = filter.push_chunk(&chunk);
                }
                // The reviewer reasons before it writes, and a grouped audit runs
                // the same reviewer model as a plain launch: forwarding the
                // deltas makes each group show the live thinking the single
                // audit already shows (#c-review-reasoning-stream).
                StreamEvent::ReasoningDelta(delta) => {
                    self.observer.publish(ReviewStreamUpdate::Reasoning(delta))
                }
                // Fold every report the provider makes into the accounting window
                // as it arrives: a group that reported usage on an intermediate
                // chunk, or whose retry resumed after an aborted attempt had
                // already reported its tokens, must not lose the earlier reports.
                StreamEvent::Usage(reported) => usage_guard.observe(&reported).await,
                StreamEvent::Done => break,
                StreamEvent::Cancelled => {
                    stream_cancelled = true;
                    break;
                }
                StreamEvent::Error(err) => {
                    failure = Some(err);
                    break;
                }
                // The retry decorator withdrew the previous attempt because it
                // broke after partial output: the filter holds a prefix of a
                // document the retry regenerates from its beginning, so it is
                // restarted instead of accumulating both payloads into one
                // unparsable body. A group streams no text to the client, so
                // there is nothing to withdraw there.
                StreamEvent::ResetStream => {
                    filter = JsonFieldStreamFilter::<ReviewFindingsOutput>::new("summary");
                }
                _ => {}
            }
        }

        // The streamed call has resolved, so the slot must stop pointing at it: a
        // later hard-cancel would otherwise abort an already finished task.
        self.operation.clear_abort_handle();

        // Account for the tokens, cost, and duration the group consumed before
        // any early return, so a failed or cancelled group is still charged.
        // Every report was already folded into the window as it arrived, so the
        // flush below charges the whole group instead of its last report alone.
        usage_guard.flush().await;
        let usage_snapshot = self.application.usage_snapshot().await;
        // Both totals are read from the authoritative ledger: the operation scope
        // the flush just closed and the session scope it was folded into.
        self.observer.publish_usage(&usage_snapshot);

        if *cancel_rx.borrow() || stream_cancelled {
            let _ = stream_task
                .abort_and_reap(&self.operation_id, self.exit_deadline)
                .await;
            return Err(GroupFailure::Cancelled);
        }
        if let Some(message) = failure {
            let _ = stream_task
                .abort_and_reap(&self.operation_id, self.exit_deadline)
                .await;
            return Err(GroupFailure::Infrastructure(message));
        }

        // Capture the raw payload before finalizing: `finish` consumes the filter,
        // while the shared parser still needs the full body to map the findings.
        let raw_buffer = filter.buffer().to_string();
        if raw_buffer.trim().is_empty() {
            return Err(GroupFailure::Domain(format!(
                "Model returned an empty review for group '{}'",
                group.title
            )));
        }
        // A payload that deserializes strictly into the review schema is
        // authoritative; one that does not is a domain failure of this group
        // rather than a fabricated finding, because the tolerant text fallback
        // would record the raw model prose as a review result.
        if let Err(err) = filter.finish() {
            return Err(GroupFailure::Domain(format!(
                "Review group '{}' returned an unparsable payload: {}",
                group.title, err
            )));
        }
        let mut findings = parse_review_output(&raw_buffer).2;

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
    repo_root: &Path,
    session: &ReviewSession,
    operation: Arc<OperationGuard>,
    provider: Arc<dyn LlmProvider>,
    observer: Arc<dyn GroupAuditObserver>,
    exit_deadline: std::time::Duration,
) -> Result<BatchReport, String> {
    if !application.owns_operation(&operation) {
        return Err("group audit requires an active operation for this session".into());
    }
    let groups = session.effective_groups();
    let steps: Vec<BatchStep> = groups
        .iter()
        .map(|group| BatchStep::new(group.id.clone(), group.title.clone()))
        .collect();

    let config = application.configuration().await.config;
    let budgets = WindowBudgets {
        files_tokens: config.context.token_budget,
    };
    let mut executor = GroupAuditExecutor {
        application,
        observer,
        exit_deadline,
        repo_root,
        session,
        groups: &groups,
        provider,
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
