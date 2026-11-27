//! Execute one admitted plan scope and publish its durable result.

use super::attempt::PlanAttempts;
use super::{needs_projection, resolve_frame as resolve_plan_frame, PlanResources};
use crate::api::{
    ApplicationEvent, ApplicationObserver, BatchOutcome, OperationGuard, PlanBatchFinished,
    PlanBatchOptions,
};
use crate::application::documents::Documents;
use crate::application::PlanExecutionEnvironment;
use crate::workflows::step::attempts::{
    ImplementationRunConfig, LeafExecutor, LeafOperations, LeafOutcome,
};
use crate::workflows::step::completion::CoderAttempt;
use crate::workflows::step::queue::{failure_reason, run_queue, QueueStop};
use async_trait::async_trait;
use std::sync::Arc;
use tauqe_core::config::ModelsConfig;
use tauqe_core::models::{ArchitectTier, ModelRole, ModelSelection};
use tauqe_core::plan::{PlanImplementation, PlanItem, PlanItemStatus, PlanScopeEvaluation};
use tauqe_core::providers::{format_session_id, StreamEvent, SESSION_PREFIX_PLAN};
use tauqe_core::stop::HardCancellation;
use tauqe_core::stop::StopToken;
use tauqe_core::workflow::batch::{QueueInterruption, SoftStopPolicy};
use tauqe_core::workflow::implementation::{
    classify_scope_stall, ExecutionScope, ImplementationFrame, PlanImplementationStop,
    RefinementRequest, RefinementResult, StepFailure, StepTurnRequest, StepTurnResult,
};
use tauqe_core::workflow::EditProtocolKind;
use tokio::sync::mpsc;

pub(crate) async fn run_batch(
    resources: &PlanResources,
    execution: &PlanExecutionEnvironment,
    plan_id: &str,
    operation: &OperationGuard,
    options: &PlanBatchOptions,
    observer: Arc<dyn ApplicationObserver>,
) -> BatchOutcome {
    let started = std::time::Instant::now();
    assert!(
        resources.step.execution.owns(operation),
        "operation belongs to another application"
    );
    let id = operation.publication_id();
    let root = resources.step.context.repository_root().await;
    let documents = Documents::new(&root);
    let _ = documents.write_active_plan(Some(plan_id.into())).await;
    let _ = resources.clear_transient_context().await;
    let plan = PlanRun {
        plan_id,
        options,
        attempts: PlanAttempts {
            resources,
            observer: observer.clone(),
            operation,
            execution,
            repo_root: &root,
            documents: &documents,
            receipts: std::sync::Mutex::new(Default::default()),
        },
    };
    let (tx, mut rx) = mpsc::channel(32);
    let cancel = operation.hard_cancellation();
    let stop = operation.stop_token();
    let run = async {
        let mut totals = RunTotals::default();
        let stop = plan.drive(&tx, &cancel, &stop, &mut totals).await;
        drop(tx);
        (stop, totals)
    };
    let relay = async {
        while let Some(event) = rx.recv().await {
            if matches!(event, StreamEvent::TierEscalated { .. }) {
                observer.publish(&id, ApplicationEvent::Stream(event));
            }
        }
    };
    let ((stop, totals), ()) = tokio::join!(run, relay);
    let usage = plan.attempts.batch_usage();

    let _ = resources.clear_transient_context().await;
    match documents.plan(plan_id).await {
        Ok(Some(plan)) => {
            observer.publish(
                &id,
                ApplicationEvent::PlanDocument {
                    plan,
                    is_save: false,
                },
            );
            if let Ok(plans) = documents.plans().await {
                observer.publish(
                    &id,
                    ApplicationEvent::PlanList {
                        plans,
                        active_id: Some(plan_id.into()),
                    },
                );
            }
        }
        Ok(None) => tracing::warn!(plan_id, "the executed plan was not found"),
        Err(error) => tracing::warn!(plan_id, %error, "failed to reload the executed plan"),
    }
    let completed = totals.succeeded;
    let outcome = match stop {
        PlanImplementationStop::Completed => BatchOutcome::Completed { steps: completed },
        PlanImplementationStop::Failed { step, reason } => BatchOutcome::Failed {
            completed,
            step_id: step.unwrap_or_else(|| plan_id.into()),
            reason,
        },
        PlanImplementationStop::Blocked { step, reason } => BatchOutcome::Blocked {
            completed,
            step_id: step.unwrap_or_else(|| plan_id.into()),
            reason,
        },
        PlanImplementationStop::Cancelled => BatchOutcome::Cancelled { completed },
        PlanImplementationStop::Stopped => BatchOutcome::Stopped { completed },
    };
    tracing::info!(
        plan_id,
        executed = totals.executed,
        succeeded = completed,
        replans = totals.replans,
        total_tokens = usage.total_tokens,
        cost = ?usage.cost,
        ?outcome,
        "Plan batch finished"
    );
    observer.publish(
        &id,
        ApplicationEvent::PlanBatchFinished(PlanBatchFinished {
            plan_id: plan_id.into(),
            outcome: outcome.clone(),
            executed: totals.executed,
            scheduled: totals.scheduled.max(totals.executed),
            elapsed: started.elapsed(),
        }),
    );
    outcome
}

struct PlanRun<'a> {
    plan_id: &'a str,
    options: &'a PlanBatchOptions,
    attempts: PlanAttempts<'a>,
}

#[derive(Default)]
struct RunTotals {
    scheduled: usize,
    executed: usize,
    succeeded: usize,
    replans: usize,
}

/// Successful repair waves on one actual Architect rung since new work completed.
/// Step identifiers are deliberately absent: rewriting the horizon is not progress.
#[derive(Default)]
struct ReplanProgress {
    tier: Option<ArchitectTier>,
    waves: usize,
}

impl ReplanProgress {
    fn next_tier(
        &self,
        models: &ModelsConfig,
        recorded: Option<&ArchitectTier>,
        limit: usize,
    ) -> Option<ArchitectTier> {
        if limit == 0 {
            return None;
        }
        let current = recorded
            .cloned()
            .unwrap_or_else(|| models.architect_tier(0));
        if self.waves < limit {
            return Some(current);
        }
        if !models.auto_level_up {
            return None;
        }
        let next = models.next_architect_tier(Some(&current));
        (next.model != current.model || next.effort != current.effort).then_some(next)
    }

    fn record_wave(&mut self, tier: ArchitectTier) {
        // Schema healing can promote inside a wave. Charge the rung that actually
        // produced the persisted horizon, not the rung that started the request.
        if self
            .tier
            .as_ref()
            .is_none_or(|previous| previous.model != tier.model || previous.effort != tier.effort)
        {
            self.waves = 0;
        }
        self.tier = Some(tier);
        self.waves += 1;
    }

    fn record_completions(&mut self, completed: usize) {
        if completed > 0 {
            self.waves = 0;
        }
    }
}

impl PlanRun<'_> {
    async fn drive(
        &self,
        stream_tx: &mpsc::Sender<StreamEvent>,
        cancel_rx: &HardCancellation,
        stop_token: &StopToken,
        totals: &mut RunTotals,
    ) -> PlanImplementationStop {
        use PlanImplementationStop as Stop;

        let step_id = self.options.step_id.as_deref();
        let scope = ExecutionScope::Plan {
            plan_id: self.plan_id.into(),
            step_id: self.options.step_id.clone(),
        };
        tracing::info!(?scope, "implementation run started");
        let mut progress = ReplanProgress::default();

        loop {
            match QueueInterruption::before_next(stop_token.state(), SoftStopPolicy::Observe) {
                Some(QueueInterruption::Cancelled) => return Stop::Cancelled,
                Some(QueueInterruption::Stopped) => return Stop::Stopped,
                None => {}
            }

            // Re-read after every queue or wave: readiness comes from the
            // durable statuses, not from the previous round's snapshot.
            let frame =
                match resolve_plan_frame(self.attempts.documents, self.plan_id, step_id).await {
                    Ok(frame) => frame,
                    Err(failure) => return failed(failure),
                };
            if step_id.is_none() && needs_projection(&frame.plan) {
                // A concept-only plan gets its first horizon without spending
                // any of the budget for repairing existing work.
                let tier = self.attempts.config().models.architect_tier(0);
                match self.run_wave(&frame, None, cancel_rx, tier).await {
                    Ok(_) => continue,
                    Err(stop) => return stop,
                }
            }

            let ready = match frame.plan.implementation.evaluate_scope(step_id) {
                PlanScopeEvaluation::Ready(steps) => {
                    steps.into_iter().map(|step| step.id).collect()
                }
                PlanScopeEvaluation::AllCompleted => return Stop::Completed,
                stall => {
                    let failure = classify_scope_stall(&frame.plan, step_id, &stall);
                    tracing::warn!(
                        ?scope,
                        domain = failure.is_domain(),
                        reason = failure.reason(),
                        "implementation scope stalled"
                    );
                    match self
                        .attempt_replan(&frame, &failure, cancel_rx, totals, &mut progress)
                        .await
                    {
                        Ok(()) => continue,
                        Err(stop) => return stop,
                    }
                }
            };

            let executor = PlanImplementationStepExecutor {
                leaf: PlanLeaf::new(
                    self.plan_id.to_owned(),
                    self.options.step_id.clone(),
                    self.attempts.documents.clone(),
                ),
                attempts: &self.attempts,
            };
            let config = ImplementationRunConfig {
                models: self.attempts.config().models.clone(),
                marker: None,
                selection: self.options.model.clone(),
                instructions: None,
                queue: ready,
            };
            let queue = run_queue(
                LeafExecutor {
                    operations: &executor,
                    config: &config,
                },
                stream_tx,
                stop_token,
                SoftStopPolicy::Observe,
            )
            .await;
            debug_assert_eq!(
                queue.executed,
                queue.succeeded + queue.skipped + queue.failed
            );
            totals.scheduled += queue.scheduled;
            totals.executed += queue.executed;
            totals.succeeded += queue.succeeded;
            // AlreadyDone is also counted as succeeded by the queue, but only
            // newly persisted completions prove that implementation advanced.
            progress.record_completions(queue.completed.len());

            match queue.stop {
                QueueStop::Completed if queue.executed == queue.skipped => return Stop::Completed,
                QueueStop::Completed => continue,
                QueueStop::Cancelled => return Stop::Cancelled,
                QueueStop::Stopped => return Stop::Stopped,
                QueueStop::Failed { step_id, outcome } => {
                    let reason = failure_reason(&outcome);
                    tracing::warn!(?scope, %step_id, %reason, "implementation run stopped on a failed step");
                    return Stop::Failed {
                        step: Some(step_id),
                        reason,
                    };
                }
                QueueStop::Blocked { step_id, reason } => {
                    let failure = StepFailure::domain(reason).with_step(step_id.clone());
                    match self
                        .attempt_replan(&frame, &failure, cancel_rx, totals, &mut progress)
                        .await
                    {
                        Ok(()) => continue,
                        Err(Stop::Failed { reason, .. } | Stop::Blocked { reason, .. }) => {
                            return Stop::Blocked {
                                step: Some(step_id),
                                reason,
                            };
                        }
                        Err(stop) => return stop,
                    }
                }
            }
        }
    }

    async fn attempt_replan(
        &self,
        frame: &ImplementationFrame,
        failure: &StepFailure,
        cancel_rx: &HardCancellation,
        totals: &mut RunTotals,
        progress: &mut ReplanProgress,
    ) -> Result<(), PlanImplementationStop> {
        if cancel_rx.is_cancelled() {
            return Err(PlanImplementationStop::Cancelled);
        }
        let Some(step) = failure.failed_step().filter(|_| failure.is_domain()) else {
            return Err(failed(failure.clone()));
        };
        // An addressed leaf or subtree cannot redesign the rest of the plan.
        if self.options.step_id.is_some() {
            return Err(failed(failure.clone()));
        }
        let max_replans = self.attempts.config().repl.max_replans;
        let models = &self.attempts.config().models;
        let recorded = frame.plan.implementation.architect.as_ref();
        let Some(tier) = progress.next_tier(models, recorded, max_replans) else {
            tracing::warn!(scope = ?frame.scope, step_id = step, max_replans, waves_without_progress = progress.waves, "implementation replanning escalation exhausted");
            return Err(PlanImplementationStop::Failed {
                step: Some(step.into()),
                reason: format!(
                    "Replanning budget exhausted (max_replans = {max_replans}) without implementation progress; Architect escalation is unavailable for step '{step}': {}",
                    failure.reason()
                ),
            });
        };
        if progress.waves >= max_replans {
            tracing::warn!(scope = ?frame.scope, from_tier = ?recorded, to_tier = ?tier, "implementation architect level up");
            self.attempts.observer.publish(
                &self.attempts.operation.publication_id(),
                ApplicationEvent::Stream(StreamEvent::TierEscalated {
                    tier: models.architect_tier_index(Some(&tier)).unwrap_or_default(),
                    model: tier.model.clone(),
                    role: ModelRole::Architect,
                    effort: tier.effort,
                }),
            );
        }

        totals.replans += 1;
        tracing::warn!(
            scope = ?frame.scope,
            step_id = step,
            replan_round = totals.replans,
            waves_without_progress = progress.waves,
            max_replans,
            reason = failure.reason(),
            "implementation replanning started"
        );
        match self.run_wave(frame, Some(failure), cancel_rx, tier).await {
            Ok(tier) => {
                progress.record_wave(tier);
                Ok(())
            }
            Err(
                PlanImplementationStop::Blocked { reason, .. }
                | PlanImplementationStop::Failed { reason, .. },
            ) => Err(PlanImplementationStop::Failed {
                step: Some(step.into()),
                reason,
            }),
            Err(stop) => Err(stop),
        }
    }

    async fn run_wave(
        &self,
        frame: &ImplementationFrame,
        failure: Option<&StepFailure>,
        cancel_rx: &HardCancellation,
        tier: ArchitectTier,
    ) -> Result<ArchitectTier, PlanImplementationStop> {
        let failed_step = failure
            .and_then(StepFailure::failed_step)
            .map(str::to_owned);

        let result = self
            .attempts
            .run_refinement(RefinementRequest {
                scope: frame.scope.clone(),
                failed_step: failed_step.clone(),
                failure: failure.map(|failure| failure.reason().to_owned()),
                selection: Some(ModelSelection::Tier {
                    model: tier.model,
                    effort: tier.effort,
                }),
                reasoning_effort: tier.effort,
                instructions: None,
            })
            .await;
        if cancel_rx.is_cancelled() {
            return Err(PlanImplementationStop::Cancelled);
        }

        match result {
            RefinementResult::Synthesized { items, .. } if items.is_empty() => {
                Err(PlanImplementationStop::Failed {
                    step: None,
                    reason: "The Architect returned a success verdict with an empty horizon".into(),
                })
            }
            RefinementResult::Synthesized { items, tier } => {
                self.attempts
                    .documents
                    .replace_plan_horizon(self.plan_id, items, tier.clone(), failed_step)
                    .await
                    .map_err(failed)?;
                publish_saved_plan(&self.attempts, self.plan_id).await;
                tracing::info!(scope = ?frame.scope, "implementation horizon synthesized");
                Ok(tier)
            }
            RefinementResult::Blocked { reason, .. } => {
                Err(PlanImplementationStop::Blocked { step: None, reason })
            }
        }
    }
}

fn failed(failure: StepFailure) -> PlanImplementationStop {
    PlanImplementationStop::Failed {
        step: failure.failed_step().map(str::to_owned),
        reason: failure.into_reason(),
    }
}

/// Publish only a persisted snapshot, before the queue advances to another step.
async fn publish_saved_plan(attempts: &PlanAttempts<'_>, plan_id: &str) {
    match attempts.documents.plan(plan_id).await {
        Ok(Some(plan)) => attempts.observer.publish(
            &attempts.operation.publication_id(),
            ApplicationEvent::PlanDocument {
                plan,
                is_save: false,
            },
        ),
        Ok(None) => tracing::warn!(plan_id, "the updated plan was not found"),
        Err(error) => tracing::warn!(plan_id, %error, "failed to reload the updated plan"),
    }
}

pub(crate) struct PlanLeaf {
    plan_id: String,
    target_step: Option<String>,
    documents: Documents,
}

impl PlanLeaf {
    pub(crate) fn new(plan_id: String, target_step: Option<String>, documents: Documents) -> Self {
        Self {
            plan_id,
            target_step,
            documents,
        }
    }

    pub(crate) async fn read(&self) -> Result<ImplementationFrame, StepFailure> {
        resolve_plan_frame(&self.documents, &self.plan_id, self.target_step.as_deref()).await
    }

    pub(crate) fn session_id(&self) -> String {
        format_session_id(SESSION_PREFIX_PLAN, &self.plan_id)
    }

    pub(crate) async fn begin(
        &self,
        step_id: &str,
    ) -> Result<Option<PlanImplementation>, StepFailure> {
        self.documents
            .write_plan_leaf(
                &self.plan_id,
                step_id,
                PlanItemStatus::InProgress,
                None,
                None,
            )
            .await
            .map(Some)
    }

    pub(crate) async fn finish(
        &self,
        step_id: &str,
        outcome: &LeafOutcome,
    ) -> Result<(), StepFailure> {
        let confirmed = match outcome {
            LeafOutcome::Cancelled(Some(receipt))
                if receipt.result == StepTurnResult::Cancelled =>
            {
                receipt.confirmed_commit()
            }
            _ => None,
        };
        let (status, blocker, commit) = match outcome {
            LeafOutcome::Completed(receipt) => {
                let StepTurnResult::Success { commit, .. } = &receipt.result else {
                    return Err(StepFailure::infrastructure(
                        "Completed leaf without a successful turn",
                    ));
                };
                (PlanItemStatus::Done, None, commit.as_deref())
            }
            LeafOutcome::Blocked(reason, _) => {
                (PlanItemStatus::Blocked, Some(reason.as_str()), None)
            }
            LeafOutcome::Cancelled(_) if confirmed.is_some() => {
                (PlanItemStatus::Done, None, confirmed)
            }
            _ => (PlanItemStatus::Todo, None, None),
        };
        let saved = self
            .documents
            .write_plan_leaf(&self.plan_id, step_id, status, blocker, commit)
            .await;
        match saved {
            Err(failure)
                if status == PlanItemStatus::Todo
                    && !matches!(outcome, LeafOutcome::Exhausted(..)) =>
            {
                tracing::warn!(
                    step_id,
                    error = failure.reason(),
                    "failed to return an implementation step to TODO"
                );
                Ok(())
            }
            result => result.map(drop),
        }
    }
}

struct PlanImplementationStepExecutor<'a> {
    leaf: PlanLeaf,
    attempts: &'a PlanAttempts<'a>,
}

#[async_trait]
impl LeafOperations for PlanImplementationStepExecutor<'_> {
    async fn read(&self) -> Result<ImplementationFrame, StepFailure> {
        self.leaf.read().await
    }

    async fn task(
        &self,
        frame: &ImplementationFrame,
        leaf: &PlanItem,
    ) -> Result<String, StepFailure> {
        tauqe_core::plan::prompt::format_plan_step_execution_prompt(
            &frame.plan,
            &leaf.id,
            EditProtocolKind::Structured,
            None,
        )
        .map_err(StepFailure::infrastructure)
    }

    fn retry_task(&self, task: &str, answer: &str, error: &str) -> String {
        tauqe_core::plan::prompt::format_plan_step_retry_prompt(
            task,
            answer,
            Some(error),
            EditProtocolKind::Structured,
            None,
        )
    }

    fn session_id(&self) -> String {
        self.leaf.session_id()
    }

    async fn begin(&self, step_id: &str) -> Result<Option<PlanImplementation>, StepFailure> {
        let implementation = self.leaf.begin(step_id).await?;
        if implementation.is_some() {
            publish_saved_plan(self.attempts, &self.leaf.plan_id).await;
        }
        Ok(implementation)
    }

    async fn finish(&self, step_id: &str, outcome: &LeafOutcome) -> Result<(), StepFailure> {
        self.leaf.finish(step_id, outcome).await?;
        publish_saved_plan(self.attempts, &self.leaf.plan_id).await;
        Ok(())
    }

    async fn attempt(&self, request: StepTurnRequest) -> CoderAttempt {
        self.attempts.run_step(request).await
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use tauqe_core::models::{ModelEntry, ModelRef, ReasoningEffort};

    fn models() -> ModelsConfig {
        ModelsConfig {
            architect: Some(vec![
                ModelEntry::new(ModelRef::openrouter("planner/shared"), None),
                ModelEntry::new(
                    ModelRef::openrouter("planner/shared"),
                    Some(ReasoningEffort::High),
                ),
            ]),
            auto_level_up: true,
            ..Default::default()
        }
    }

    #[test]
    fn two_waves_are_checked_by_the_coder_before_promotion() {
        let models = models();
        let first = models.architect_tier(0);
        let second = models.architect_tier(1);
        let mut progress = ReplanProgress::default();
        for _ in 0..2 {
            assert_eq!(
                progress.next_tier(&models, Some(&first), 2),
                Some(first.clone())
            );
            progress.record_wave(first.clone());
            progress.record_completions(0);
        }
        assert_eq!(
            progress.next_tier(&models, Some(&first), 2),
            Some(second.clone())
        );
        progress.record_wave(second.clone());
        assert_eq!(progress.waves, 1);
        assert_eq!(
            progress.next_tier(&models, Some(&second), 2),
            Some(second.clone())
        );
        progress.record_wave(second.clone());
        assert!(progress.next_tier(&models, Some(&second), 2).is_none());
    }

    #[test]
    fn only_new_completions_reset_the_stall_budget_without_downgrading() {
        let models = models();
        let tier = models.architect_tier(1);
        let mut progress = ReplanProgress::default();
        for _ in 0..4 {
            progress.record_wave(tier.clone());
            progress.record_wave(tier.clone());
            progress.record_completions(0);
            assert!(progress.next_tier(&models, Some(&tier), 2).is_none());
            progress.record_completions(1);
            assert_eq!(
                progress.next_tier(&models, Some(&tier), 2),
                Some(tier.clone())
            );
        }
    }

    #[test]
    fn internal_synthesis_promotion_charges_the_actual_rung() {
        let models = models();
        let first = models.architect_tier(0);
        let second = models.architect_tier(1);
        let mut progress = ReplanProgress::default();
        progress.record_wave(first);
        progress.record_wave(second.clone());
        assert_eq!(progress.waves, 1);
        assert_eq!(progress.next_tier(&models, Some(&second), 2), Some(second));
    }

    #[test]
    fn disabled_replanning_and_unavailable_promotions_do_not_run_another_wave() {
        let mut models = models();
        let first = models.architect_tier(0);
        let mut progress = ReplanProgress::default();
        assert!(progress.next_tier(&models, Some(&first), 0).is_none());
        progress.record_wave(first.clone());
        models.auto_level_up = false;
        assert!(progress.next_tier(&models, Some(&first), 1).is_none());
        models.auto_level_up = true;
        models.architect.as_mut().unwrap()[0].level_up = Some(false);
        assert!(progress.next_tier(&models, Some(&first), 1).is_none());
    }
}
