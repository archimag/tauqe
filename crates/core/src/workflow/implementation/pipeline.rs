//! Batch execution of one implementation horizon.
//!
//! The pipeline is the single driver of an implementation run. It resolves the
//! horizon of its [`ExecutionScope`] from the storage that owns it, projects a
//! horizon just in time when the scope owns none yet, schedules the ready leaves
//! in dependency order, drives the core [`BatchExecution`] loop through the
//! [`PlanImplementationStepExecutor`], and reprojects the remaining horizon
//! through the [`ImplementationHost`] whenever a step fails with a diagnostic the
//! Architect can repair.
//!
//! Three invariants keep the pipeline independent of the workflow that owns the
//! work:
//!
//! - **The storage is the single source of truth.** Every step lifecycle
//!   transition is written through the storage that owns the horizon — a plan's
//!   steps through [`PlanStorage`], a review group's sub-steps through
//!   [`ReviewStorage`] — and the graph is re-resolved from disk before every
//!   round. The pipeline never carries a mutable copy of the graph, so a status a
//!   step committed is always observed by the next evaluation.
//! - **A wave reprojects, never regenerates.** A replanning wave receives the
//!   failed step and its diagnostic, the Architect ladder decides its rung, and
//!   the merge primitive of the storage preserves the immutable `Done`
//!   foundation, so finished work is never re-derived by the model.
//! - **Readiness is derived, never guessed.** The leaves of a round are exactly
//!   the ready set of the shared scope evaluator; a scope that cannot start is
//!   classified into a repairable dependency stall or a terminal lifecycle stall
//!   instead of being scheduled blindly.
//!
//! A degenerate single-finding run owns no persisted horizon, so it executes
//! exactly one round and is never reprojected; the finding itself carries the
//! verdict of that round, because it is the only durable record the run has.
//!
//! The lifecycle of a degenerate single-finding run belongs to its step
//! executor: taking the step marks the finding `InProgress` and a reset returns
//! it to `Todo`, so the session states which finding is executing right now.
//! The verdict is never written there. It is stated exactly once, when the
//! horizon reaches its terminal verdict: an architectural group closes the
//! findings it still holds open once its step graph is `Done`, and a degenerate
//! single-finding run states whether the run addressed the finding or skipped
//! it.

use std::path::Path;

use tauqe_protocol::{
    ArchitectTier, IssueStatus, ModelSelection, PlanItem, PlanItemStatus, PlanScopeEvaluation,
};
use tokio::sync::{mpsc, watch};

use crate::config::AppConfig;
use crate::plan::prompt::resolve_architect_tier;
use crate::plan::storage::PlanStorage;
use crate::providers::StreamEvent;
use crate::review::storage::ReviewStorage;
use crate::workflow::batch::{BatchExecution, BatchStep, BatchStepStatus, BatchStop};
use crate::workflow::implementation::{
    classify_scope_stall, ExecutionScope, ImplementationFrame, ImplementationHost,
    ImplementationRunConfig, PlanImplementationStepExecutor, RefinementRequest, RefinementResult,
    StepFailure,
};

/// Knobs a caller declares for one implementation run.
#[derive(Debug, Clone, Default)]
pub struct PlanImplementationOptions {
    /// Coder selection the run starts from, resolved against the coder ladder.
    pub selection: Option<ModelSelection>,
    /// Additional developer directives forwarded to every step turn and wave.
    pub instructions: Option<String>,
    /// Turn marker bound to the tagged control channel.
    pub marker: Option<String>,
}

/// Terminal reason an implementation run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanImplementationStop {
    /// Every executable leaf of the horizon finished.
    Completed,
    /// A failure the pipeline cannot repair by reprojecting the horizon.
    Failed {
        /// The step the failure is attributed to, when it is attributable.
        step: Option<String>,
        /// Diagnostic of the failure.
        reason: String,
    },
    /// A step cannot proceed until the developer resolves the stated blocker.
    Blocked {
        /// The step the blocker is attributed to, when it is attributable.
        step: Option<String>,
        /// Diagnostic of the blocker.
        reason: String,
    },
    /// The run observed a cancellation request.
    Cancelled,
    /// The run observed a soft-stop request and drained the running step.
    Stopped,
}

/// Terminal report of one implementation run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanImplementationReport {
    /// The scope whose horizon the run executed.
    pub scope: ExecutionScope,
    /// Terminal reason the run stopped.
    pub stop: PlanImplementationStop,
    /// Leaves scheduled for execution across every round of the run.
    pub scheduled: usize,
    /// Leaves the batch executed, including the skipped and the failed ones.
    pub executed: usize,
    /// Executed leaves that satisfied their success contract.
    pub succeeded: usize,
    /// Executed leaves that filtered themselves out of the run.
    pub skipped: usize,
    /// Executed leaves that failed, blocked, or were cancelled.
    pub failed: usize,
    /// Architect waves the run spent reprojecting its horizon.
    ///
    /// The initial just-in-time projection of a horizon the scope did not own yet
    /// is not a replan, so it is not counted here.
    pub replans: usize,
}

impl PlanImplementationReport {
    /// Whether the run executed every remaining leaf of its horizon.
    pub fn is_success(&self) -> bool {
        matches!(self.stop, PlanImplementationStop::Completed)
    }
}

/// Batch driver of one implementation horizon.
///
/// A pipeline is created for exactly one [`ExecutionScope`] and runs until the
/// horizon is complete, a step reports a terminal failure, or the run observes a
/// cancellation or a soft stop. It owns a snapshot of the configuration, so a run
/// that outlives the configuration lock of its caller still executes every step
/// against one consistent set of ladders and budgets.
pub struct PlanImplementationPipeline<'a> {
    repo_root: &'a Path,
    scope: ExecutionScope,
    host: &'a dyn ImplementationHost,
    config: AppConfig,
    options: PlanImplementationOptions,
}

impl<'a> PlanImplementationPipeline<'a> {
    /// Creates the pipeline of one implementation run.
    pub fn new(
        repo_root: &'a Path,
        scope: ExecutionScope,
        host: &'a dyn ImplementationHost,
        config: AppConfig,
        options: PlanImplementationOptions,
    ) -> Self {
        Self {
            repo_root,
            scope,
            host,
            config,
            options,
        }
    }

    /// Drives the horizon of the scope to a terminal report.
    ///
    /// The run alternates between deriving the ready set of the horizon and
    /// executing it. A round that completes rediscovers the horizon, so a leaf
    /// that a completed step unblocked joins the next round instead of waiting for
    /// the caller. `cancel_rx` is hard cancellation and wins over `soft_stop_rx`,
    /// which lets the running step finish its turn, verification, and commit before
    /// the queue halts.
    ///
    /// The terminal verdict settles the review entity the scope resolves from
    /// before the report leaves the pipeline, so a caller never observes a
    /// finished horizon beside findings that stayed open.
    pub async fn run(
        &self,
        stream_tx: &mpsc::Sender<StreamEvent>,
        cancel_rx: &watch::Receiver<bool>,
        soft_stop_rx: &watch::Receiver<bool>,
    ) -> PlanImplementationReport {
        let report = self.drive(stream_tx, cancel_rx, soft_stop_rx).await;
        self.settle_completed_scope(&report);
        report
    }

    /// Drives the ready leaves of the horizon round after round.
    ///
    /// A round resolves the horizon from the storage that owns it, schedules its
    /// ready set, folds the batch outcome into the run totals, and decides the
    /// next move: continue, replan around a repairable stall, or return.
    async fn drive(
        &self,
        stream_tx: &mpsc::Sender<StreamEvent>,
        cancel_rx: &watch::Receiver<bool>,
        soft_stop_rx: &watch::Receiver<bool>,
    ) -> PlanImplementationReport {
        let mut totals = PipelineTotals::default();
        let mut replans = 0usize;

        tracing::info!(scope = ?self.scope, "implementation run started");

        loop {
            // Hard cancellation wins over a soft stop when both are pending, so an
            // explicit reset is never downgraded to a graceful stop.
            if *cancel_rx.borrow() {
                return self.report(totals, replans, PlanImplementationStop::Cancelled);
            }
            if *soft_stop_rx.borrow() {
                return self.report(totals, replans, PlanImplementationStop::Stopped);
            }

            // The horizon is re-resolved from the storage that owns it before every
            // round, so the pipeline never decides the next round from a stale copy
            // of the graph.
            let frame = match self.scope.resolve(self.repo_root) {
                Ok(frame) => frame,
                Err(failure) => {
                    return self.report(
                        totals,
                        replans,
                        PlanImplementationStop::Failed {
                            step: failure.failed_step().map(str::to_string),
                            reason: failure.into_reason(),
                        },
                    );
                }
            };

            if frame.plan.items.is_empty() {
                // A plan that carries an architectural concept but no synthesized
                // horizon, and an architectural review group, still have no leaf to
                // schedule: the Architect projects the first horizon just in time,
                // and the resulting leaves join this same loop. A scope that carries
                // neither a concept nor a step has nothing to decompose.
                if frame.plan.concept.is_empty() {
                    return self.report(totals, replans, PlanImplementationStop::Completed);
                }
                match self.run_wave(&frame, None, cancel_rx).await {
                    WaveOutcome::Synthesized => continue,
                    WaveOutcome::Blocked { reason } => {
                        return self.report(
                            totals,
                            replans,
                            PlanImplementationStop::Blocked { step: None, reason },
                        );
                    }
                    WaveOutcome::Failed { reason } => {
                        return self.report(
                            totals,
                            replans,
                            PlanImplementationStop::Failed { step: None, reason },
                        );
                    }
                    WaveOutcome::Cancelled => {
                        return self.report(totals, replans, PlanImplementationStop::Cancelled);
                    }
                }
            }

            let ready: Vec<String> = match frame.plan.evaluate_scope(None) {
                PlanScopeEvaluation::Ready(steps) => {
                    steps.into_iter().map(|step| step.id).collect()
                }
                PlanScopeEvaluation::AllCompleted => {
                    return self.report(totals, replans, PlanImplementationStop::Completed);
                }
                stall => {
                    // A stalled scope is never scheduled blindly: the shared
                    // classifier decides whether reprojecting the horizon can repair
                    // it or whether only the developer can.
                    let failure = classify_scope_stall(&frame.plan, None, &stall);
                    tracing::warn!(
                        scope = ?self.scope,
                        domain = failure.is_domain(),
                        reason = failure.reason(),
                        "implementation scope stalled"
                    );
                    if !failure.is_domain() {
                        return self.report(
                            totals,
                            replans,
                            PlanImplementationStop::Failed {
                                step: failure.failed_step().map(str::to_string),
                                reason: failure.into_reason(),
                            },
                        );
                    }
                    let failed_step = failure.failed_step().map(str::to_string);
                    match self
                        .attempt_replan(&frame, &failure, cancel_rx, &mut replans)
                        .await
                    {
                        ReplanOutcome::Resumed => continue,
                        ReplanOutcome::Cancelled => {
                            return self.report(totals, replans, PlanImplementationStop::Cancelled);
                        }
                        ReplanOutcome::Unrepaired { reason } => {
                            return self.report(
                                totals,
                                replans,
                                PlanImplementationStop::Failed {
                                    step: failed_step,
                                    reason,
                                },
                            );
                        }
                    }
                }
            };

            let steps: Vec<BatchStep> = ready
                .iter()
                .map(|id| BatchStep::new(id.clone(), format!("Execute plan step {id}")))
                .collect();
            totals.scheduled += steps.len();

            let run_config = ImplementationRunConfig {
                models: self.config.models.clone(),
                protocol: self.config.repl.protocol,
                marker: self.options.marker.clone(),
                selection: self.options.selection.clone(),
                instructions: self.options.instructions.clone(),
                queue: ready,
            };
            let mut executor = PlanImplementationStepExecutor::new(
                self.repo_root,
                self.scope.clone(),
                self.host,
                run_config,
            );
            let batch = BatchExecution::new()
                .run(&steps, &mut executor, stream_tx, cancel_rx, soft_stop_rx)
                .await;

            totals.executed += batch.executed.len();
            for entry in &batch.executed {
                match entry.outcome.status {
                    BatchStepStatus::Succeeded => totals.succeeded += 1,
                    BatchStepStatus::Skipped => totals.skipped += 1,
                    BatchStepStatus::Failed
                    | BatchStepStatus::Blocked
                    | BatchStepStatus::Cancelled => totals.failed += 1,
                }
            }

            match batch.stop {
                BatchStop::Completed => {
                    // A degenerate single finding owns no persisted step status, so
                    // its one leaf can never become `Done` on disk: the round it
                    // executed is the whole run, and re-evaluating would launch the
                    // same finding again.
                    if matches!(self.scope, ExecutionScope::ReviewFinding { .. }) {
                        return self.report(totals, replans, PlanImplementationStop::Completed);
                    }
                    // A round that scheduled leaves but produced neither a success
                    // nor a failure made no progress; ending the run keeps an
                    // executor that silently skipped its work from spinning the
                    // pipeline forever.
                    if batch
                        .executed
                        .iter()
                        .all(|entry| entry.outcome.status == BatchStepStatus::Skipped)
                    {
                        return self.report(totals, replans, PlanImplementationStop::Completed);
                    }
                    continue;
                }
                BatchStop::Cancelled => {
                    return self.report(totals, replans, PlanImplementationStop::Cancelled);
                }
                BatchStop::Stopped => {
                    return self.report(totals, replans, PlanImplementationStop::Stopped);
                }
                BatchStop::Failed { step_id } => {
                    // A failure the executor does not report as a semantic block is
                    // terminal for the run: a transport fault, a missing credential,
                    // or a working tree the attempt never reverted would meet the
                    // same condition on the next attempt, so no wave is spent.
                    let reason = batch
                        .executed
                        .last()
                        .and_then(|entry| entry.outcome.error.clone())
                        .unwrap_or_else(|| "the step failed without diagnostics".to_string());
                    tracing::warn!(
                        scope = ?self.scope,
                        step_id = step_id.as_str(),
                        reason = reason.as_str(),
                        "implementation run stopped on a failed step"
                    );
                    return self.report(
                        totals,
                        replans,
                        PlanImplementationStop::Failed {
                            step: Some(step_id),
                            reason,
                        },
                    );
                }
                BatchStop::Blocked { step_id, reason } => {
                    // A semantic block is the attributed domain failure the Architect
                    // can repair: the step stated that the approved decision cannot be
                    // implemented, so the remaining horizon is reprojected around it.
                    let failure = StepFailure::domain(reason).with_step(step_id.clone());
                    match self
                        .attempt_replan(&frame, &failure, cancel_rx, &mut replans)
                        .await
                    {
                        ReplanOutcome::Resumed => continue,
                        ReplanOutcome::Cancelled => {
                            return self.report(totals, replans, PlanImplementationStop::Cancelled);
                        }
                        ReplanOutcome::Unrepaired { reason } => {
                            return self.report(
                                totals,
                                replans,
                                PlanImplementationStop::Blocked {
                                    step: Some(step_id),
                                    reason,
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    /// Applies the completion discipline of the scope the run belongs to.
    ///
    /// A local plan needs no settlement: its steps are its whole state. A review
    /// scope does, because its findings are entities distinct from the horizon
    /// that resolves them, and the pipeline is the only place that knows the
    /// horizon reached its end.
    ///
    /// The lifecycle of a running step is not part of the settlement: the step
    /// executor already wrote `InProgress` when it took the finding and `Todo`
    /// when it reset it. The settlement states the verdict alone, exactly once,
    /// so no second writer touches the status of a finding.
    fn settle_completed_scope(&self, report: &PlanImplementationReport) {
        match &self.scope {
            ExecutionScope::Plan { .. } => {}
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => self.close_completed_group(review_id, group_id),
            ExecutionScope::ReviewFinding { review_id, item_id } => {
                self.settle_finding(report, review_id, *item_id);
            }
        }
    }

    /// Closes the findings of a group whose horizon reached the aggregate `Done`
    /// status.
    ///
    /// The trigger reads the persisted horizon, never the report of the run: the
    /// group is complete exactly when its step graph is, and the graph is the
    /// storage's answer. A horizon that is still running, stalled, or carries no
    /// step at all therefore closes nothing.
    fn close_completed_group(&self, review_id: &str, group_id: &str) {
        let complete = self
            .scope
            .resolve(self.repo_root)
            .is_ok_and(|frame| frame.plan.implementation.status() == PlanItemStatus::Done);
        if !complete {
            return;
        }

        match ReviewStorage::close_group_implementation(
            self.repo_root,
            review_id,
            group_id,
            GROUP_COMPLETION_NOTE,
        ) {
            Ok(Some(closed)) => tracing::info!(
                review_id = review_id,
                group_id = group_id,
                closed = closed,
                "review group implementation closed"
            ),
            Ok(None) => tracing::warn!(
                review_id = review_id,
                group_id = group_id,
                "the review group completion was not persisted: the group was not found"
            ),
            Err(err) => tracing::warn!(
                review_id = review_id,
                group_id = group_id,
                error = %err,
                "the review group completion was not persisted"
            ),
        }
    }

    /// States the verdict of the single finding a degenerate run implements.
    ///
    /// A degenerate horizon owns no persisted step graph, so the finding is the
    /// only durable record of the run: a completed run closes it as `Fixed` with
    /// the result of the change set, and a run whose step could not be implemented
    /// as approved closes it as `Skipped` with the diagnostic. Cancellation and a
    /// soft stop are not verdicts on the finding, so an interrupted run writes no
    /// verdict and keeps the lifecycle its step executor recorded.
    ///
    /// The commit is never restated by the verdict: the step executor wrote the
    /// commit of the completed step on the finding, and the settlement states the
    /// status and the note only, so a finished finding keeps the exact pointer to
    /// the change set that resolved it.
    fn settle_finding(&self, report: &PlanImplementationReport, review_id: &str, item_id: u32) {
        let (status, note) = match &report.stop {
            PlanImplementationStop::Completed => (
                IssueStatus::Fixed,
                format!(
                    "Addressed by the implementation run: {} of {} scheduled step(s) succeeded",
                    report.succeeded, report.scheduled
                ),
            ),
            PlanImplementationStop::Failed { reason, .. }
            | PlanImplementationStop::Blocked { reason, .. } => (
                IssueStatus::Skipped,
                format!("The implementation run could not address this finding: {reason}"),
            ),
            PlanImplementationStop::Cancelled | PlanImplementationStop::Stopped => return,
        };

        let result =
            ReviewStorage::update_session_item(self.repo_root, review_id, item_id, |issue| {
                issue.status = status;
                // The commit the step executor recorded is the durable pointer of
                // the change set, so the verdict states the status and the note
                // and leaves it untouched.
                // A note the developer already stated is never overwritten: the run
                // records its verdict on a finding that carries none.
                if !carries_note(issue.note.as_deref()) {
                    issue.note = Some(note.clone());
                }
            });

        match result {
            Ok(Some(_)) => tracing::info!(
                review_id = review_id,
                item_id = item_id,
                status = status.as_str(),
                "review finding settled"
            ),
            Ok(None) => tracing::warn!(
                review_id = review_id,
                item_id = item_id,
                "the review finding verdict was not persisted: the finding was not found"
            ),
            Err(err) => tracing::warn!(
                review_id = review_id,
                item_id = item_id,
                error = %err,
                "the review finding verdict was not persisted"
            ),
        }
    }

    /// Applies the replanning policy to one attributed failure.
    ///
    /// A wave is spent only on a domain failure that names the step whose approach
    /// failed, because that is the only diagnostic the Architect can reproject the
    /// remaining horizon from. The failure kind is decisive: an infrastructure
    /// failure would meet the same broken transport on the next attempt, and a
    /// working tree the failed attempt never reverted would build the reprojected
    /// horizon on lines nobody verified.
    async fn attempt_replan(
        &self,
        frame: &ImplementationFrame,
        failure: &StepFailure,
        cancel_rx: &watch::Receiver<bool>,
        replans: &mut usize,
    ) -> ReplanOutcome {
        let Some(step) = failure.failed_step() else {
            return ReplanOutcome::Unrepaired {
                reason: failure.reason().to_string(),
            };
        };
        if !failure.is_domain() {
            return ReplanOutcome::Unrepaired {
                reason: failure.reason().to_string(),
            };
        }
        // A degenerate single finding owns no persisted horizon, so there is no
        // graph the Architect could reproject and write back.
        if matches!(self.scope, ExecutionScope::ReviewFinding { .. }) {
            return ReplanOutcome::Unrepaired {
                reason: format!(
                    "Step '{}' cannot be reprojected: a single review finding owns no persisted horizon. {}",
                    step,
                    failure.reason()
                ),
            };
        }

        let max_replans = self.config.repl.max_replans;
        if *replans >= max_replans {
            tracing::warn!(
                scope = ?self.scope,
                step_id = step,
                max_replans = max_replans,
                "implementation replanning limit reached"
            );
            // The group keeps the diagnosis of the approach that failed, so the
            // durable session states why its horizon was not reprojected.
            self.record_group_blocker(Some(failure.reason()));
            return ReplanOutcome::Unrepaired {
                reason: format!(
                    "Replanning budget exhausted (max_replans = {}) for step '{}': {}",
                    max_replans,
                    step,
                    failure.reason()
                ),
            };
        }

        *replans += 1;
        tracing::warn!(
            scope = ?self.scope,
            step_id = step,
            replan_round = *replans,
            max_replans = max_replans,
            reason = failure.reason(),
            "implementation replanning started"
        );

        match self.run_wave(frame, Some(failure), cancel_rx).await {
            WaveOutcome::Synthesized => ReplanOutcome::Resumed,
            WaveOutcome::Blocked { reason } | WaveOutcome::Failed { reason } => {
                ReplanOutcome::Unrepaired { reason }
            }
            WaveOutcome::Cancelled => ReplanOutcome::Cancelled,
        }
    }

    /// Runs one Architect wave over the horizon of the frame.
    ///
    /// The wave is the same Architect operation for an initial projection and for
    /// a reprojection: the difference is the differential it receives, never the
    /// contour it drives or the storage primitive that persists its answer.
    async fn run_wave(
        &self,
        frame: &ImplementationFrame,
        failure: Option<&StepFailure>,
        cancel_rx: &watch::Receiver<bool>,
    ) -> WaveOutcome {
        let implementation = &frame.plan.implementation;
        let failed_step = failure
            .and_then(|failure| failure.failed_step())
            .map(str::to_string);

        // Tier resolution and the lazy level-up invariant are core-owned domain
        // policy shared with every other Architect operation, so the pipeline never
        // re-derives the ladder locally. A first synthesis of a horizon starts from
        // the recorded rung, and only a repeated failure of the same step escalates
        // it.
        let (tier, level_up) = resolve_architect_tier(
            &self.config,
            implementation.architect.as_ref(),
            None,
            failed_step.as_deref(),
            implementation.last_replanned_step.as_deref(),
        );
        if let Some(level) = &level_up {
            tracing::warn!(
                scope = ?self.scope,
                from_tier = ?level.from,
                to_tier = ?level.to,
                "implementation architect level up"
            );
        }

        let request = RefinementRequest {
            scope: self.scope.clone(),
            failed_step: failed_step.clone(),
            failure: failure.map(|failure| failure.reason().to_string()),
            // The wave runs on exactly the rung the ladder handed out: the resolved
            // pair travels as the explicit selection of the request, so the host
            // executes it without re-deriving the policy.
            selection: Some(ModelSelection::Specific(tier.model)),
            reasoning_effort: tier.effort,
            instructions: self.options.instructions.clone(),
            marker: self.options.marker.clone(),
            cancel_rx: cancel_rx.clone(),
        };

        let result = self.host.run_refinement(request).await;

        // A cancellation observed while the contour settled still ends the run as a
        // cancellation instead of a provider failure.
        if *cancel_rx.borrow() {
            return WaveOutcome::Cancelled;
        }

        match result {
            RefinementResult::Synthesized { items, tier } => {
                if items.is_empty() {
                    return WaveOutcome::Failed {
                        reason: "The Architect returned a success verdict with an empty horizon"
                            .to_string(),
                    };
                }
                match self.persist_horizon(items, tier, failed_step) {
                    Ok(()) => {
                        // A group that owns a fresh horizon is no longer stopped, so a
                        // client never renders a stale blocker beside a synthesized
                        // plan.
                        self.record_group_blocker(None);
                        tracing::info!(
                            scope = ?self.scope,
                            "implementation horizon synthesized"
                        );
                        WaveOutcome::Synthesized
                    }
                    Err(reason) => WaveOutcome::Failed { reason },
                }
            }
            RefinementResult::Blocked { reason, .. } => {
                // The blocker belongs to the group that has to resolve it, not to the
                // findings its horizon manifests.
                self.record_group_blocker(Some(reason.as_str()));
                WaveOutcome::Blocked { reason }
            }
        }
    }

    /// Writes a synthesized horizon through the storage that owns it.
    ///
    /// Both primitives preserve the immutable `Done` foundation, reject a horizon
    /// that closes a dependency cycle, record the Architect configuration that
    /// produced it, and persist the reprojected step together with it, so the next
    /// wave reads its escalation input from disk.
    fn persist_horizon(
        &self,
        items: Vec<PlanItem>,
        tier: ArchitectTier,
        last_replanned_step: Option<String>,
    ) -> Result<(), String> {
        match &self.scope {
            ExecutionScope::Plan { plan_id } => PlanStorage::replace_uncompleted_horizon(
                self.repo_root,
                plan_id,
                items,
                tier,
                last_replanned_step,
            )
            .map(|_| ())
            .map_err(|err| err.to_string()),
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => {
                let updated = ReviewStorage::replace_uncompleted_group_subplan(
                    self.repo_root,
                    review_id,
                    group_id,
                    items,
                    tier,
                    last_replanned_step,
                )
                .map_err(|err| err.to_string())?;
                updated.map(|_| ()).ok_or_else(|| {
                    format!(
                        "Review group '{}' of session '{}' is not stored; the synthesized horizon was not persisted",
                        group_id, review_id
                    )
                })
            }
            ExecutionScope::ReviewFinding { .. } => {
                Err("A single review finding owns no persisted horizon to rewrite".to_string())
            }
        }
    }

    /// Records or clears the blocker of a review group.
    ///
    /// A plan carries no blocker of its own, and a degenerate single-finding run
    /// owns no group, so both scopes resolve to nothing. The write is best-effort:
    /// it accompanies a verdict the run already reached, so a storage problem is
    /// journaled instead of replacing that verdict.
    fn record_group_blocker(&self, reason: Option<&str>) {
        let ExecutionScope::ReviewGroup {
            review_id,
            group_id,
        } = &self.scope
        else {
            return;
        };
        match ReviewStorage::set_group_blocked_reason(self.repo_root, review_id, group_id, reason) {
            Ok(Some(_)) => {}
            Ok(None) => tracing::warn!(
                review_id = review_id.as_str(),
                group_id = group_id.as_str(),
                "the review group blocker was not persisted: the group was not found"
            ),
            Err(err) => tracing::warn!(
                review_id = review_id.as_str(),
                group_id = group_id.as_str(),
                error = %err,
                "the review group blocker was not persisted"
            ),
        }
    }

    /// Builds the terminal report of the run.
    fn report(
        &self,
        totals: PipelineTotals,
        replans: usize,
        stop: PlanImplementationStop,
    ) -> PlanImplementationReport {
        PlanImplementationReport {
            scope: self.scope.clone(),
            stop,
            scheduled: totals.scheduled,
            executed: totals.executed,
            succeeded: totals.succeeded,
            skipped: totals.skipped,
            failed: totals.failed,
            replans,
        }
    }
}

/// Note recorded on the findings a completed group horizon closes.
///
/// It states why the findings became `Fixed`: the horizon the group owns
/// finished. A finding that already carries a note keeps it.
const GROUP_COMPLETION_NOTE: &str =
    "Resolved by the completed implementation horizon of the review group";

/// Whether a finding already carries a resolution note.
///
/// A note the developer stated is never rewritten by a settlement: the note of
/// the run is recorded only on a finding that carries none.
fn carries_note(note: Option<&str>) -> bool {
    note.map(str::trim).is_some_and(|text| !text.is_empty())
}

/// Accumulated step counters of a pipeline run.
#[derive(Debug, Clone, Copy, Default)]
struct PipelineTotals {
    scheduled: usize,
    executed: usize,
    succeeded: usize,
    skipped: usize,
    failed: usize,
}

/// Terminal result of one Architect wave.
enum WaveOutcome {
    /// The wave persisted a projected horizon.
    Synthesized,
    /// The Architect reported that the horizon cannot be projected as stated.
    Blocked {
        /// Diagnostic of the blocked wave.
        reason: String,
    },
    /// The wave produced no horizon this run could persist.
    Failed {
        /// Diagnostic of the failed wave.
        reason: String,
    },
    /// The run observed a cancellation request while the wave ran.
    Cancelled,
}

/// Decision of the replanning policy for one attributed failure.
enum ReplanOutcome {
    /// A wave reprojected the horizon; the run continues from it.
    Resumed,
    /// The failure is terminal, and the stated reason explains why.
    Unrepaired {
        /// Diagnostic of the unrepaired failure.
        reason: String,
    },
    /// The run observed a cancellation request.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tauqe_protocol::review::{IssueStatus, IssueType, ReviewGroupKind, ReviewIssue};
    use tauqe_protocol::{
        ArchitectTier, ConceptItem, ModelEntry, ModelRef, ModelSelection, Plan, PlanItem,
        PlanItemStatus, ReasoningEffort, ReviewGroup, ReviewItem, ReviewSession,
    };
    use tempfile::tempdir;

    use super::*;
    use crate::config::ModelsConfig;
    use crate::workflow::implementation::{StepTurnRequest, StepTurnResult};

    /// Identifier of the plan every plan-scope fixture stores.
    const PLAN_ID: &str = "pipeline-plan";
    /// Identifier of the review session every review-scope fixture stores.
    const REVIEW_ID: &str = "pipeline-review";
    /// Identifier of the architectural group a review fixture declares.
    const GROUP_ID: &str = "arch-1";

    /// One recorded Architect wave, reduced to the fields a test asserts on.
    #[derive(Debug, Clone)]
    struct WaveRecord {
        failed_step: Option<String>,
        failure: Option<String>,
        selection: Option<ModelSelection>,
        reasoning_effort: Option<ReasoningEffort>,
    }

    /// State the scripted host mutates while a run drives it.
    struct HostState {
        steps: VecDeque<StepTurnResult>,
        waves: VecDeque<RefinementResult>,
        refinements: Vec<WaveRecord>,
        steps_run: usize,
        soft_stop: Option<(watch::Sender<bool>, usize)>,
        /// Repository root the host re-reads the stored review from while a step
        /// runs, when a test observes the persisted finding status mid-run.
        store_probe: Option<std::path::PathBuf>,
        /// Status the observed finding carried the last time a step ran.
        observed_status: Option<IssueStatus>,
    }

    /// Host that replays scripted step results and Architect waves.
    struct ScriptedHost {
        state: Mutex<HostState>,
    }

    impl ScriptedHost {
        fn new(steps: Vec<StepTurnResult>, waves: Vec<RefinementResult>) -> Self {
            Self {
                state: Mutex::new(HostState {
                    steps: steps.into(),
                    waves: waves.into(),
                    refinements: Vec::new(),
                    steps_run: 0,
                    soft_stop: None,
                    store_probe: None,
                    observed_status: None,
                }),
            }
        }

        /// Raises the soft-stop signal once `after` steps have run, simulating a
        /// graceful request arriving in the middle of a batch.
        fn with_soft_stop(self, sender: watch::Sender<bool>, after: usize) -> Self {
            if let Ok(mut state) = self.state.lock() {
                state.soft_stop = Some((sender, after));
            }
            self
        }

        /// Makes the host re-read the stored review session from `root` before
        /// replaying every step, so a test can observe the persisted status of a
        /// finding while its step runs.
        fn with_store_probe(self, root: std::path::PathBuf) -> Self {
            if let Ok(mut state) = self.state.lock() {
                state.store_probe = Some(root);
            }
            self
        }

        /// Returns the status the observed finding carried during the last step.
        fn observed_status(&self) -> Option<IssueStatus> {
            self.state
                .lock()
                .ok()
                .and_then(|state| state.observed_status)
        }
        fn refinements(&self) -> Vec<WaveRecord> {
            self.state.lock().unwrap().refinements.clone()
        }

        fn steps_run(&self) -> usize {
            self.state.lock().unwrap().steps_run
        }
    }

    #[async_trait]
    impl ImplementationHost for ScriptedHost {
        async fn run_step(&self, _request: StepTurnRequest) -> StepTurnResult {
            let mut state = self.state.lock().unwrap();
            state.steps_run += 1;
            let observed = state.store_probe.as_deref().and_then(|root| {
                ReviewStorage::load_review(root, REVIEW_ID)
                    .ok()
                    .flatten()
                    .and_then(|session| session.find_item(7).map(|item| item.status))
            });
            state.observed_status = observed;
            let run = state.steps_run;
            if let Some((sender, after)) = state.soft_stop.as_ref() {
                if run >= *after {
                    let _ = sender.send(true);
                }
            }
            state.steps.pop_front().unwrap_or_else(|| {
                StepTurnResult::Failed(StepFailure::infrastructure("no scripted step result"))
            })
        }

        async fn run_refinement(&self, request: RefinementRequest) -> RefinementResult {
            let mut state = self.state.lock().unwrap();
            state.refinements.push(WaveRecord {
                failed_step: request.failed_step.clone(),
                failure: request.failure.clone(),
                selection: request.selection.clone(),
                reasoning_effort: request.reasoning_effort,
            });
            state
                .waves
                .pop_front()
                .unwrap_or_else(|| RefinementResult::Blocked {
                    reason: "the scripted host has no refinement wave left".to_string(),
                    tier: ArchitectTier::from(ModelRef::openrouter("architect/fallback")),
                })
        }
    }

    /// Channels a pipeline run observes and a test drives.
    struct Channels {
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_tx: watch::Sender<bool>,
        cancel_rx: watch::Receiver<bool>,
        soft_stop_tx: watch::Sender<bool>,
        soft_stop_rx: watch::Receiver<bool>,
    }

    impl Channels {
        fn new() -> Self {
            let (stream_tx, _stream_rx) = mpsc::channel(8);
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let (soft_stop_tx, soft_stop_rx) = watch::channel(false);
            Self {
                stream_tx,
                cancel_tx,
                cancel_rx,
                soft_stop_tx,
                soft_stop_rx,
            }
        }

        fn cancel(&self) {
            let _ = self.cancel_tx.send(true);
        }
    }

    /// Builds a configuration whose Architect ladder is the given list.
    fn config_with_ladder(architect: Vec<ModelEntry>, max_replans: usize) -> AppConfig {
        let mut config = AppConfig {
            models: ModelsConfig {
                primary: vec![ModelEntry::from(ModelRef::openrouter("base/primary"))],
                architect: Some(architect),
                ..ModelsConfig::default()
            },
            ..AppConfig::default()
        };
        config.repl.max_replans = max_replans;
        config
    }

    /// Builds the single Architect rung most fixtures resolve.
    fn tier_entry(model: &str) -> ModelEntry {
        ModelEntry::from(ModelRef::openrouter(model))
    }

    /// Builds the single concept node of a plan fixture.
    fn concept() -> Vec<ConceptItem> {
        vec![ConceptItem {
            id: "c-1".to_string(),
            title: "Invariant".to_string(),
            details: None,
            children: Vec::new(),
        }]
    }

    /// Builds one executable leaf that declares its target file.
    fn leaf(id: &str, status: PlanItemStatus) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: format!("Step {id}"),
            status,
            files: vec![format!("src/step-{id}.rs")],
            ..PlanItem::default()
        }
    }

    /// Builds the plan scope fixture of a test.
    fn plan_with(
        concept: Vec<ConceptItem>,
        items: Vec<PlanItem>,
        architect: Option<ArchitectTier>,
        last_replanned_step: Option<String>,
    ) -> Plan {
        let mut plan = Plan::from_parts(
            PLAN_ID.to_string(),
            "Pipeline Plan".to_string(),
            Some("Drive one implementation horizon.".to_string()),
            0,
            0,
            concept,
            items,
        );
        plan.implementation.architect = architect;
        plan.implementation.last_replanned_step = last_replanned_step;
        plan
    }

    /// Builds the scope of the stored plan fixture.
    fn plan_scope() -> ExecutionScope {
        ExecutionScope::Plan {
            plan_id: PLAN_ID.to_string(),
        }
    }

    /// Builds a review session holding one architectural group and one finding.
    fn review_session() -> ReviewSession {
        let finding = ReviewItem::from(ReviewIssue {
            id: 7,
            title: "Guard the parser".to_string(),
            uuid: None,
            commit: None,
            group: Some(GROUP_ID.to_string()),
            model: None,
            issue_type: IssueType::Bug,
            status: IssueStatus::Todo,
            description: "The parser panics on truncated input.".to_string(),
            recommendations: Vec::new(),
            files: vec!["src/parser.rs".to_string()],
            readonly_files: Vec::new(),
            note: None,
        });
        ReviewSession {
            id: REVIEW_ID.to_string(),
            title: "Pipeline audit".to_string(),
            items: vec![finding],
            groups: vec![ReviewGroup {
                id: GROUP_ID.to_string(),
                title: "Unified error handling".to_string(),
                description: Some("Every module invents its own error type.".to_string()),
                kind: ReviewGroupKind::Architectural,
                ..ReviewGroup::default()
            }],
            ..ReviewSession::default()
        }
    }

    /// Runs one pipeline over the given scope.
    async fn run_scope(
        root: &Path,
        scope: ExecutionScope,
        host: &ScriptedHost,
        config: AppConfig,
        channels: &Channels,
    ) -> PlanImplementationReport {
        PlanImplementationPipeline::new(
            root,
            scope,
            host,
            config,
            PlanImplementationOptions::default(),
        )
        .run(
            &channels.stream_tx,
            &channels.cancel_rx,
            &channels.soft_stop_rx,
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_horizon_is_synthesized_just_in_time_and_executed() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &plan_with(concept(), Vec::new(), None, None)).unwrap();

        let tier = ArchitectTier::from(ModelRef::openrouter("architect/one"));
        let host = ScriptedHost::new(
            vec![StepTurnResult::Success {
                summary: "the horizon is in place".to_string(),
                commit: None,
            }],
            vec![RefinementResult::Synthesized {
                items: vec![leaf("1", PlanItemStatus::Todo)],
                tier: tier.clone(),
            }],
        );
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            plan_scope(),
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Completed);
        assert_eq!(report.succeeded, 1);
        // The initial projection is not a replan.
        assert_eq!(report.replans, 0);

        let waves = host.refinements();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].failed_step, None);
        assert_eq!(
            waves[0].selection,
            Some(ModelSelection::Specific(ModelRef::openrouter(
                "architect/one"
            )))
        );

        let stored = PlanStorage::load_plan(dir.path(), PLAN_ID)
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Done);
        assert_eq!(stored.implementation.architect, Some(tier));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_first_failure_of_a_step_keeps_the_recorded_architect() {
        let dir = tempdir().unwrap();
        let recorded = ArchitectTier::new(
            ModelRef::openrouter("architect/one"),
            Some(ReasoningEffort::Low),
        );
        PlanStorage::save_plan(
            dir.path(),
            &plan_with(
                concept(),
                vec![leaf("1", PlanItemStatus::Todo)],
                Some(recorded),
                // A different step was reprojected last, so the failure of step
                // `1` is the first failure of its own approach.
                Some("0".to_string()),
            ),
        )
        .unwrap();

        let host = ScriptedHost::new(
            vec![
                StepTurnResult::Blocked("the approved structure cannot express it".to_string()),
                StepTurnResult::Success {
                    summary: "reprojected".to_string(),
                    commit: None,
                },
            ],
            vec![RefinementResult::Synthesized {
                items: vec![leaf("2", PlanItemStatus::Todo)],
                tier: ArchitectTier::from(ModelRef::openrouter("architect/one")),
            }],
        );
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            plan_scope(),
            &host,
            config_with_ladder(
                vec![
                    ModelEntry::new(
                        ModelRef::openrouter("architect/one"),
                        Some(ReasoningEffort::Low),
                    ),
                    ModelEntry::new(
                        ModelRef::openrouter("architect/two"),
                        Some(ReasoningEffort::High),
                    ),
                ],
                2,
            ),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Completed);
        assert_eq!(report.replans, 1);

        let waves = host.refinements();
        assert_eq!(waves[0].failed_step.as_deref(), Some("1"));
        assert_eq!(
            waves[0].failure.as_deref(),
            Some("the approved structure cannot express it")
        );
        assert_eq!(
            waves[0].selection,
            Some(ModelSelection::Specific(ModelRef::openrouter(
                "architect/one"
            )))
        );
        assert_eq!(waves[0].reasoning_effort, Some(ReasoningEffort::Low));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_repeated_failure_of_the_same_step_escalates_the_architect() {
        let dir = tempdir().unwrap();
        let tier_one = ArchitectTier::new(
            ModelRef::openrouter("architect/one"),
            Some(ReasoningEffort::Low),
        );
        let tier_two = ArchitectTier::new(
            ModelRef::openrouter("architect/two"),
            Some(ReasoningEffort::High),
        );
        PlanStorage::save_plan(
            dir.path(),
            &plan_with(
                concept(),
                vec![leaf("1", PlanItemStatus::Todo)],
                Some(tier_one),
                // The horizon was already reprojected around step `1`, so the next
                // failure of that very step is a repeated failure.
                Some("1".to_string()),
            ),
        )
        .unwrap();

        let host = ScriptedHost::new(
            vec![
                StepTurnResult::Blocked("the approved structure cannot express it".to_string()),
                StepTurnResult::Success {
                    summary: "reprojected".to_string(),
                    commit: None,
                },
            ],
            vec![RefinementResult::Synthesized {
                items: vec![leaf("2", PlanItemStatus::Todo)],
                tier: tier_two.clone(),
            }],
        );
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            plan_scope(),
            &host,
            config_with_ladder(
                vec![
                    ModelEntry::new(
                        ModelRef::openrouter("architect/one"),
                        Some(ReasoningEffort::Low),
                    ),
                    ModelEntry::new(
                        ModelRef::openrouter("architect/two"),
                        Some(ReasoningEffort::High),
                    ),
                ],
                2,
            ),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Completed);
        assert_eq!(report.replans, 1);

        let waves = host.refinements();
        assert_eq!(waves[0].failed_step.as_deref(), Some("1"));
        assert_eq!(
            waves[0].selection,
            Some(ModelSelection::Specific(ModelRef::openrouter(
                "architect/two"
            )))
        );
        assert_eq!(waves[0].reasoning_effort, Some(ReasoningEffort::High));

        let stored = PlanStorage::load_plan(dir.path(), PLAN_ID)
            .unwrap()
            .unwrap();
        assert_eq!(stored.implementation.architect, Some(tier_two));
        assert_eq!(
            stored.implementation.last_replanned_step.as_deref(),
            Some("1")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_replanning_budget_is_bounded() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(
            dir.path(),
            &plan_with(
                concept(),
                vec![leaf("1", PlanItemStatus::Todo)],
                Some(ArchitectTier::from(ModelRef::openrouter("architect/one"))),
                None,
            ),
        )
        .unwrap();

        let host = ScriptedHost::new(
            vec![
                StepTurnResult::Blocked("first failure".to_string()),
                StepTurnResult::Blocked("second failure".to_string()),
            ],
            vec![RefinementResult::Synthesized {
                items: vec![leaf("2", PlanItemStatus::Todo)],
                tier: ArchitectTier::from(ModelRef::openrouter("architect/one")),
            }],
        );
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            plan_scope(),
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 1),
            &channels,
        )
        .await;

        assert_eq!(report.replans, 1);
        assert_eq!(host.refinements().len(), 1);
        match &report.stop {
            PlanImplementationStop::Blocked { step, reason } => {
                assert_eq!(step.as_deref(), Some("2"));
                assert!(
                    reason.contains("max_replans = 1"),
                    "the diagnostic must name the budget: {reason}"
                );
                assert!(
                    reason.contains("second failure"),
                    "the diagnostic must keep the cause: {reason}"
                );
            }
            other => panic!("unexpected stop: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dependency_stall_is_reprojected_by_the_architect() {
        let dir = tempdir().unwrap();
        let mut dependent = leaf("2", PlanItemStatus::Todo);
        dependent.depends_on = vec!["1".to_string()];
        PlanStorage::save_plan(
            dir.path(),
            &plan_with(
                concept(),
                vec![leaf("1", PlanItemStatus::Cancelled), dependent],
                Some(ArchitectTier::from(ModelRef::openrouter("architect/one"))),
                None,
            ),
        )
        .unwrap();

        let host = ScriptedHost::new(
            vec![StepTurnResult::Success {
                summary: "replanned".to_string(),
                commit: None,
            }],
            vec![RefinementResult::Synthesized {
                items: vec![leaf("3", PlanItemStatus::Todo)],
                tier: ArchitectTier::from(ModelRef::openrouter("architect/one")),
            }],
        );
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            plan_scope(),
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Completed);
        assert_eq!(report.replans, 1);
        let waves = host.refinements();
        // A dependency stall is a domain failure of the stalled step itself, so
        // the wave receives it as the failed approach.
        assert_eq!(waves[0].failed_step.as_deref(), Some("2"));

        let stored = PlanStorage::load_plan(dir.path(), PLAN_ID)
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("3").unwrap().status, PlanItemStatus::Done);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lifecycle_stall_ends_the_run_without_a_wave() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(
            dir.path(),
            &plan_with(
                concept(),
                vec![leaf("1", PlanItemStatus::Blocked)],
                Some(ArchitectTier::from(ModelRef::openrouter("architect/one"))),
                None,
            ),
        )
        .unwrap();

        let host = ScriptedHost::new(Vec::new(), Vec::new());
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            plan_scope(),
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        // A blocked leaf is a lifecycle precondition no reprojection can repair.
        assert!(host.refinements().is_empty());
        assert_eq!(host.steps_run(), 0);
        match &report.stop {
            PlanImplementationStop::Failed { reason, .. } => assert!(
                reason.contains("#1"),
                "the diagnostic must name the blocked step: {reason}"
            ),
            other => panic!("unexpected stop: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_cancellation_ends_the_run_before_the_first_step() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(
            dir.path(),
            &plan_with(concept(), vec![leaf("1", PlanItemStatus::Todo)], None, None),
        )
        .unwrap();

        let host = ScriptedHost::new(Vec::new(), Vec::new());
        let channels = Channels::new();
        channels.cancel();

        let report = run_scope(
            dir.path(),
            plan_scope(),
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Cancelled);
        assert_eq!(report.executed, 0);
        assert_eq!(host.steps_run(), 0);
        let stored = PlanStorage::load_plan(dir.path(), PLAN_ID)
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Todo);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_soft_stop_requested_mid_run_keeps_the_completed_step() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(
            dir.path(),
            &plan_with(
                concept(),
                vec![
                    leaf("1", PlanItemStatus::Todo),
                    leaf("2", PlanItemStatus::Todo),
                ],
                None,
                None,
            ),
        )
        .unwrap();

        let channels = Channels::new();
        let host = ScriptedHost::new(
            vec![
                StepTurnResult::Success {
                    summary: "first step".to_string(),
                    commit: None,
                },
                StepTurnResult::Success {
                    summary: "second step".to_string(),
                    commit: None,
                },
            ],
            Vec::new(),
        )
        .with_soft_stop(channels.soft_stop_tx.clone(), 1);

        let report = run_scope(
            dir.path(),
            plan_scope(),
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Stopped);
        assert_eq!(report.succeeded, 1);
        assert_eq!(host.steps_run(), 1);

        let stored = PlanStorage::load_plan(dir.path(), PLAN_ID)
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Done);
        assert_eq!(stored.find_item("2").unwrap().status, PlanItemStatus::Todo);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_group_horizon_is_synthesized_and_its_blocker_cleared() {
        let dir = tempdir().unwrap();
        ReviewStorage::save_session(dir.path(), &review_session()).unwrap();
        // The group enters the run carrying the blocker of an earlier attempt.
        ReviewStorage::set_group_blocked_reason(
            dir.path(),
            REVIEW_ID,
            GROUP_ID,
            Some("an earlier attempt stalled"),
        )
        .unwrap();

        let host = ScriptedHost::new(
            vec![StepTurnResult::Success {
                summary: "root cause removed".to_string(),
                commit: None,
            }],
            vec![RefinementResult::Synthesized {
                items: vec![leaf("s-1", PlanItemStatus::Todo)],
                tier: ArchitectTier::from(ModelRef::openrouter("architect/one")),
            }],
        );
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            ExecutionScope::ReviewGroup {
                review_id: REVIEW_ID.to_string(),
                group_id: GROUP_ID.to_string(),
            },
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Completed);
        assert_eq!(report.succeeded, 1);

        let stored = ReviewStorage::load_review(dir.path(), REVIEW_ID)
            .unwrap()
            .unwrap();
        let group = stored
            .groups
            .iter()
            .find(|group| group.id == GROUP_ID)
            .expect("the group is stored");
        assert_eq!(group.implementation.items.len(), 1);
        assert_eq!(group.implementation.items[0].status, PlanItemStatus::Done);
        // A group that owns a fresh horizon is no longer stopped.
        assert_eq!(group.blocked_reason, None);
        // The completed horizon closes the finding the group manifests.
        assert_eq!(stored.items[0].status, IssueStatus::Fixed);
        assert!(stored.items[0]
            .resolution_notes
            .as_deref()
            .is_some_and(|note| note.contains("Resolved by the completed implementation")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocked_group_synthesis_is_recorded_on_the_group() {
        let dir = tempdir().unwrap();
        ReviewStorage::save_session(dir.path(), &review_session()).unwrap();

        let host = ScriptedHost::new(
            Vec::new(),
            vec![RefinementResult::Blocked {
                reason: "the root cause cannot be removed as stated".to_string(),
                tier: ArchitectTier::from(ModelRef::openrouter("architect/one")),
            }],
        );
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            ExecutionScope::ReviewGroup {
                review_id: REVIEW_ID.to_string(),
                group_id: GROUP_ID.to_string(),
            },
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        assert_eq!(
            report.stop,
            PlanImplementationStop::Blocked {
                step: None,
                reason: "the root cause cannot be removed as stated".to_string(),
            }
        );

        let stored = ReviewStorage::load_review(dir.path(), REVIEW_ID)
            .unwrap()
            .unwrap();
        let group = stored
            .groups
            .iter()
            .find(|group| group.id == GROUP_ID)
            .expect("the group is stored");
        assert_eq!(
            group.blocked_reason.as_deref(),
            Some("the root cause cannot be removed as stated")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_finding_runs_as_a_degenerate_step_and_closes_itself() {
        let dir = tempdir().unwrap();
        store_review_with_unrelated_finding(dir.path());

        let host = ScriptedHost::new(
            vec![StepTurnResult::Success {
                summary: "the finding is addressed".to_string(),
                commit: Some("abc1234".to_string()),
            }],
            Vec::new(),
        );
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            ExecutionScope::ReviewFinding {
                review_id: REVIEW_ID.to_string(),
                item_id: 7,
            },
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Completed);
        assert_eq!(report.succeeded, 1);
        // The degenerate horizon runs exactly once and is never reprojected.
        assert_eq!(host.steps_run(), 1);
        assert!(host.refinements().is_empty());

        let stored = ReviewStorage::load_review(dir.path(), REVIEW_ID)
            .unwrap()
            .unwrap();
        // The run states the verdict of the finding it addressed and never
        // touches a finding of another group.
        let target = stored
            .find_item(7)
            .expect("the addressed finding is stored");
        assert_eq!(target.status, IssueStatus::Fixed);
        assert_eq!(
            target.commit.as_deref(),
            Some("abc1234"),
            "the settlement keeps the commit the step executor recorded"
        );
        assert!(target
            .resolution_notes
            .as_deref()
            .is_some_and(|note| note.contains("Addressed by the implementation run")));
        assert_eq!(stored.find_item(8).unwrap().status, IssueStatus::Todo);
        assert!(stored.find_item(8).unwrap().resolution_notes.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_degenerate_finding_reads_in_progress_mid_run_and_fixed_after_settlement() {
        let dir = tempdir().unwrap();
        store_review_with_unrelated_finding(dir.path());

        let host = ScriptedHost::new(
            vec![StepTurnResult::Success {
                summary: "the finding is addressed".to_string(),
                commit: None,
            }],
            Vec::new(),
        )
        .with_store_probe(dir.path().to_path_buf());
        let channels = Channels::new();

        let report = run_scope(
            dir.path(),
            ExecutionScope::ReviewFinding {
                review_id: REVIEW_ID.to_string(),
                item_id: 7,
            },
            &host,
            config_with_ladder(vec![tier_entry("architect/one")], 2),
            &channels,
        )
        .await;

        assert_eq!(report.stop, PlanImplementationStop::Completed);
        // The step executor marks the finding as taken before the turn runs, so
        // the session states what is executing while the model works.
        assert_eq!(host.observed_status(), Some(IssueStatus::InProgress));

        // The verdict itself is stated exactly once, by the settlement.
        let stored = ReviewStorage::load_review(dir.path(), REVIEW_ID)
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item(7).unwrap().status, IssueStatus::Fixed);
    }
    /// Stores the review fixture with a second finding in another group, so a
    /// settlement can be observed against a finding it must never touch.
    fn store_review_with_unrelated_finding(repo_root: &Path) {
        let mut session = review_session();
        let mut unrelated = session.items[0].clone();
        unrelated.id = 8;
        unrelated.group = Some("local".to_string());
        unrelated.status = IssueStatus::Todo;
        session.items.push(unrelated);
        ReviewStorage::save_session(repo_root, &session).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_degenerate_finding_is_skipped_with_its_diagnostic() {
        let dir = tempdir().unwrap();
        store_review_with_unrelated_finding(dir.path());

        let host = ScriptedHost::new(
            vec![StepTurnResult::Failed(StepFailure::domain(
                "the parser guard cannot be introduced as approved",
            ))],
            Vec::new(),
        );
        let channels = Channels::new();
        // A single rung with a single attempt: one content failure exhausts the
        // ladder instead of retrying it on the same model.
        let mut config = config_with_ladder(vec![tier_entry("architect/one")], 2);
        config.models.coder = Some(vec![ModelEntry {
            model: ModelRef::openrouter("base/primary"),
            effort: None,
            attempts: Some(1),
            level_up: None,
        }]);

        let report = run_scope(
            dir.path(),
            ExecutionScope::ReviewFinding {
                review_id: REVIEW_ID.to_string(),
                item_id: 7,
            },
            &host,
            config,
            &channels,
        )
        .await;

        assert!(matches!(
            &report.stop,
            PlanImplementationStop::Failed { .. }
        ));

        let stored = ReviewStorage::load_review(dir.path(), REVIEW_ID)
            .unwrap()
            .unwrap();
        let target = stored
            .find_item(7)
            .expect("the addressed finding is stored");
        assert_eq!(target.status, IssueStatus::Skipped);
        assert!(
            target.resolution_notes.as_deref().is_some_and(
                |note| note.contains("the parser guard cannot be introduced as approved")
            )
        );
        assert_eq!(stored.find_item(8).unwrap().status, IssueStatus::Todo);
        // The degenerate horizon owns no stored graph, so no wave is spent.
        assert!(host.refinements().is_empty());
    }
}
