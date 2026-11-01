//! Server-side implementation host.
//!
//! The engine's unified implementation machinery
//! (`tauqe_core::workflow::implementation`) owns *what* a horizon does: which
//! leaf runs, on which coder rung, with which prompt, and when a failure
//! justifies an Architect wave. This module owns *how* one of those turns is
//! executed inside the daemon: context preloading, the provider turn with its
//! streamed telemetry and usage window, and the classification of the turn
//! result into the engine's failure taxonomy.
//!
//! One implementation serves every workflow that owns a step graph, so a plan
//! batch and a review run cannot drift apart in how they reach a provider, how
//! they publish telemetry, or how they interpret a blocked, aborted, cancelled,
//! or unverified turn.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::config::AppConfig;
use tauqe_core::plan::prompt::{
    build_plan_horizon_system_prompt, build_plan_horizon_user_prompt, PlanHorizonDiagnostics,
};
use tauqe_core::providers::{format_session_id, SESSION_PREFIX_ARCHITECT, SESSION_PREFIX_REVIEW};
use tauqe_core::workflow::fsm::structured::{StructuredRefineOutcome, StructuredRefineStatus};
use tauqe_core::workflow::implementation::{
    ExecutionScope, ImplementationHost, RefinementRequest, RefinementResult, StepFailure,
    StepTurnRequest, StepTurnResult,
};
use tauqe_core::workflow::WorkflowOptions;
use tauqe_protocol::model::{TurnWorkContext, TurnWorkQueue};
use tauqe_protocol::{
    events, ArchitectTier, ModelFinishedEvent, ModelRole, ModelSelection, ModelStartedEvent,
    ModelTurnPlanContext, ReasoningEffort, TurnIntent,
};

use crate::handlers::architect::run_contour_synthesis;
use crate::handlers::common::{
    append_developer_directives, clear_auto_context, emit, preload_declared_files,
    turn_applied_edits, turn_left_unreverted_changes,
};
use crate::handlers::model::{build_apply_policy, execute_model_turn, StepTurnOptions};
use crate::state::{AppState, OperationGuard};

/// Budget diagnostic an Architect wave reports when it outgrows its context.
const HORIZON_BUDGET_HINT: &str =
    "The plan is too broad and should be manually decomposed or scoped down.";

/// Turn seam between the engine's implementation machinery and the daemon.
///
/// The host borrows the operation that owns the whole run, so the batch slot
/// never lapses between members and a single cancellation reaches every step and
/// every wave of the run through one channel.
pub struct ServerImplementationHost {
    state: Arc<AppState>,
    operation: Arc<OperationGuard>,
    /// Configuration snapshot of the run, taken when the batch started.
    config: AppConfig,
    repo_root: PathBuf,
}

impl ServerImplementationHost {
    /// Creates the host of one implementation run.
    pub fn new(
        state: Arc<AppState>,
        operation: Arc<OperationGuard>,
        config: AppConfig,
        repo_root: PathBuf,
    ) -> Self {
        Self {
            state,
            operation,
            config,
            repo_root,
        }
    }

    /// Declares the plan turn coordinates of a plan step.
    ///
    /// A review scope carries no plan coordinates: its turns are addressed by
    /// the review entities the runner executes.
    fn plan_context(scope: &ExecutionScope, queue: &TurnWorkQueue) -> Option<ModelTurnPlanContext> {
        let plan_id = match scope {
            ExecutionScope::Plan { plan_id } => plan_id,
            ExecutionScope::ReviewGroup { .. } | ExecutionScope::ReviewFinding { .. } => {
                return None
            }
        };
        let step_id = queue
            .items
            .get(queue.active_index)
            .map(|item| item.id.clone())
            .unwrap_or_default();
        Some(ModelTurnPlanContext {
            plan_id: plan_id.clone(),
            step_id,
            batch_step_ids: queue.items.iter().map(|item| item.id.clone()).collect(),
        })
    }

    /// Declares the work the running step executes from typed fields.
    ///
    /// The review runners still execute their own steps, so only a plan scope
    /// declares a work context here; a review step renders its marker through the
    /// runner that owns the review queue.
    fn work_context(
        scope: &ExecutionScope,
        title: &str,
        queue: TurnWorkQueue,
    ) -> Option<TurnWorkContext> {
        match scope {
            ExecutionScope::Plan { plan_id } => Some(TurnWorkContext::PlanStep {
                plan_id: plan_id.clone(),
                plan_title: title.to_string(),
                queue,
            }),
            ExecutionScope::ReviewGroup { .. } | ExecutionScope::ReviewFinding { .. } => None,
        }
    }

    /// Resolves the Architect rung a wave runs on from the request.
    ///
    /// The pipeline already resolved the ladder and applied the lazy level-up
    /// invariant, so the request names the exact rung to execute; the host never
    /// re-derives the escalation policy locally.
    fn architect_tier(
        &self,
        selection: Option<&ModelSelection>,
        effort: Option<ReasoningEffort>,
    ) -> ArchitectTier {
        let model = match selection {
            Some(selection) => self.config.resolve_model(selection),
            None => self.config.models.architect_tier(0).model,
        };
        ArchitectTier::new(model, effort)
    }
}

#[async_trait]
impl ImplementationHost for ServerImplementationHost {
    async fn run_step(&self, request: StepTurnRequest) -> StepTurnResult {
        let StepTurnRequest {
            scope,
            title,
            queue,
            prompt,
            files,
            readonly_files,
            selection,
            reasoning_effort,
            instructions,
            marker,
            session_id,
            cancel_rx,
        } = request;

        // Every attempt starts from a clean transient layer: the files a previous
        // step preloaded describe work that is already finished.
        clear_auto_context(&self.state).await;
        preload_declared_files(&self.state, &self.repo_root, &files, &readonly_files).await;

        let plan_context = Self::plan_context(&scope, &queue);
        let work_context = Self::work_context(&scope, &title, queue);
        let mut prompt = prompt;
        append_developer_directives(&mut prompt, instructions.as_deref());

        let policy = build_apply_policy(&self.state, self.config.develop.workflow);
        let options = StepTurnOptions {
            intent: TurnIntent::IsolatedStep,
            custom_selection: Some(selection),
            plan_context,
            work_context,
            reasoning_effort,
            marker,
            session_id,
            // The step borrows the batch-level registration, so the operation
            // slot keeps owning the run across every member.
            shared_operation: Some(Arc::clone(&self.operation)),
            phase_out: Some(self.state.out.clone()),
        };

        let turn = match execute_model_turn(&self.state, prompt, policy, options).await {
            Ok(turn) => turn,
            Err(err) => {
                return StepTurnResult::Failed(StepFailure::infrastructure(err.error.message))
            }
        };

        // A semantic block is a verdict about the approved decision, never a
        // content failure of the step: it travels as its own outcome so the
        // pipeline engages the Architect instead of climbing the coder ladder.
        if let Some(reason) = turn.blocked.clone() {
            return StepTurnResult::Blocked(reason);
        }
        // An unconditional abort needs an action only the developer can perform in
        // the host environment, so it stops the queue without spending a wave.
        if let Some(abort) = turn.abort.clone() {
            return StepTurnResult::Abort(abort.to_message());
        }
        if turn.is_success() && turn_applied_edits(&turn) {
            let summary = match &turn.result {
                Some(tauqe_protocol::ModelResult::Edit { summary, .. }) => summary.clone(),
                _ => String::new(),
            };
            return StepTurnResult::Success { summary };
        }
        if turn.is_cancelled || *cancel_rx.borrow() {
            return StepTurnResult::Cancelled;
        }
        // A turn without a model result ended on an infrastructure failure: the
        // provider stream broke, the connection was interrupted, or the workflow
        // failed before proposing anything. A stronger model would meet the same
        // broken transport, so the failure is never classified as a domain one.
        if turn.result.is_none() {
            let reason = turn.error.clone().unwrap_or_else(|| {
                "The step turn ended without producing a model result".to_string()
            });
            return StepTurnResult::Failed(StepFailure::infrastructure(reason));
        }
        // A failed change set the turn could not roll back leaves the working tree
        // carrying the bytes of the aborted attempt. Retrying on a dirty tree would
        // build the next attempt on lines nobody verified.
        if turn_left_unreverted_changes(&turn) {
            let cause = turn.error.clone().unwrap_or_default();
            return StepTurnResult::Failed(StepFailure::unreverted(format!(
                "The failed change set was not rolled back to the step checkpoint, so the working tree carries unverified lines. Run `git status` to inspect them and `git checkout -- .` to restore the step base. {cause}"
            )));
        }
        let reason = turn.error.clone().unwrap_or_else(|| {
            "The step turn finished without applying a verified change set".to_string()
        });
        StepTurnResult::Failed(StepFailure::domain(reason))
    }

    async fn run_refinement(&self, request: RefinementRequest) -> RefinementResult {
        let tier = self.architect_tier(request.selection.as_ref(), request.reasoning_effort);
        let frame = match request.scope.resolve(&self.repo_root) {
            Ok(frame) => frame,
            Err(failure) => {
                return RefinementResult::Blocked {
                    reason: failure.into_reason(),
                    tier,
                }
            }
        };

        let op_id = format!("op-{}", self.operation.id());
        let mut options = WorkflowOptions::from_app_config(&self.config);
        // A wave is an isolated design dialogue of the entity that owns the
        // horizon: the session is keyed by the plan or the review audit, never by
        // the interactive REPL, so a step turn can never poison the dialogue cache.
        options.session_id = Some(match &frame.scope {
            ExecutionScope::Plan { plan_id } => {
                format_session_id(SESSION_PREFIX_ARCHITECT, plan_id)
            }
            ExecutionScope::ReviewGroup { review_id, .. }
            | ExecutionScope::ReviewFinding { review_id, .. } => {
                format_session_id(SESSION_PREFIX_REVIEW, review_id)
            }
        });

        emit(
            &self.state.out,
            events::MODEL_STARTED,
            serde_json::to_value(ModelStartedEvent {
                operation_id: op_id.clone(),
                model: tier.model.clone(),
                role: Some(ModelRole::Architect),
                alias: None,
                effort: tier.effort,
                plan_context: None,
                work_context: None,
                is_batch: true,
            })
            .ok(),
        );

        // The wave reprojects the remaining horizon around the approach that
        // failed, so the diagnostic of that failure travels into the task: without
        // it the Architect would regenerate the very horizon that already failed.
        let diagnostics = request
            .failed_step
            .as_deref()
            .map(|step| PlanHorizonDiagnostics {
                blocked_step_id: step,
                blocked_reason: request.failure.as_deref(),
                toolchain_status: None,
            });
        let mut task = build_plan_horizon_system_prompt();
        task.push_str("\n\n---\n\n");
        task.push_str(&build_plan_horizon_user_prompt(
            &frame.plan,
            diagnostics,
            request.instructions.as_deref(),
        ));
        let intro_header = format!(
            "# Automated Plan Replanning: [{}] {}\n*Reprojecting the remaining horizon of this implementation...*\n\n---\n\n",
            frame.plan.id, frame.plan.title
        );

        let context_snapshot = self.state.context.read().await.clone();
        let (contour, summary) = run_contour_synthesis(
            &self.state,
            &self.operation,
            &op_id,
            task,
            context_snapshot,
            options,
            self.config.models.clone(),
            tier.clone(),
            intro_header,
            HORIZON_BUDGET_HINT.to_string(),
        )
        .await;

        if !summary.cancelled {
            emit(
                &self.state.out,
                events::MODEL_FINISHED,
                serde_json::to_value(ModelFinishedEvent {
                    operation_id: op_id,
                    full_text: summary.assistant_text.clone(),
                })
                .ok(),
            );
        }

        let outcome = match contour {
            Ok(outcome) => outcome,
            Err(err) => {
                return RefinementResult::Blocked {
                    reason: summary.error.unwrap_or_else(|| err.to_string()),
                    tier,
                }
            }
        };

        let StructuredRefineOutcome {
            output,
            tier: synthesized_tier,
            status,
        } = outcome;
        match status {
            StructuredRefineStatus::Synthesized => match output {
                Some(output) if !output.items.is_empty() => RefinementResult::Synthesized {
                    items: output.items,
                    tier: synthesized_tier,
                },
                _ => RefinementResult::Blocked {
                    reason: "The Architect reported a synthesized horizon without any step"
                        .to_string(),
                    tier: synthesized_tier,
                },
            },
            StructuredRefineStatus::Blocked { reason } => RefinementResult::Blocked {
                reason,
                tier: synthesized_tier,
            },
            StructuredRefineStatus::BudgetExceeded { diagnostic } => RefinementResult::Blocked {
                reason: diagnostic,
                tier: synthesized_tier,
            },
            StructuredRefineStatus::Cancelled => RefinementResult::Blocked {
                reason: "The Architect wave was cancelled".to_string(),
                tier: synthesized_tier,
            },
        }
    }
}
