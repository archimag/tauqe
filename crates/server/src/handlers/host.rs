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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::config::AppConfig;
use tauqe_core::outer_docs::OuterDocsRegistry;
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
use tokio::sync::watch;

use crate::handlers::architect::run_contour_synthesis;
use crate::handlers::common::{
    append_developer_directives, append_outer_docs_diagnostics, cleanup_transient_context, emit,
    preload_declared_files, preload_declared_outer_docs, turn_applied_edits,
    turn_left_unreverted_changes, OuterDocsRefusal,
};
use crate::handlers::model::{build_apply_policy, execute_model_turn, StepTurnOptions};
use crate::handlers::review::publish_review;
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
    /// Every scope names its own work kind, so the interface never infers the task
    /// from a title: a plan step carries the plan coordinates, and both review
    /// scopes carry the review address with the queue the run advances through — a
    /// review group through its unified horizon, a single finding as a degenerate
    /// run.
    ///
    /// The typed request carries only the title of the horizon a scope owns, which
    /// is the plan title, the group title, or the finding title; the title of the
    /// review session itself is not part of it. Both review variants therefore
    /// address the review by the identifier the client can resolve, instead of
    /// inventing a session title nobody declared.
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
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => Some(TurnWorkContext::ReviewGroupStep {
                review_id: review_id.clone(),
                review_title: review_id.clone(),
                group_id: group_id.clone(),
                group_title: title.to_string(),
                queue,
            }),
            ExecutionScope::ReviewFinding { review_id, .. } => Some(TurnWorkContext::ReviewStep {
                review_id: review_id.clone(),
                review_title: review_id.clone(),
                queue,
            }),
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

    /// Resolves the external documents the active step of the run declares.
    ///
    /// The step identity comes from the queue the request carries, so the
    /// documents belong to the leaf the turn actually executes rather than to the
    /// horizon as a whole. The horizon is re-resolved from the storage that owns
    /// it, exactly as the pipeline resolves it before launching the step, so the
    /// host never carries a second copy of the step graph. A scope that cannot be
    /// read declares nothing: the pipeline reports that failure through the
    /// diagnostic it owns.
    fn declared_outer_docs(
        scope: &ExecutionScope,
        queue: &TurnWorkQueue,
        repo_root: &Path,
    ) -> Vec<String> {
        let Ok(frame) = scope.resolve(repo_root) else {
            return Vec::new();
        };
        let Some(step_id) = queue.active().map(|item| item.id.as_str()) else {
            return Vec::new();
        };
        frame
            .plan
            .find_item(step_id)
            .map(|step| step.outer_docs.clone())
            .unwrap_or_default()
    }

    /// Materializes and mounts the external documents the active step declares.
    ///
    /// A mounted document reaches the model through the read-only OuterDocs
    /// context layer, while a document the harness could not deliver becomes a
    /// diagnostic of the step prompt instead of failing the step.
    async fn preload_step_outer_docs(
        &self,
        scope: &ExecutionScope,
        queue: &TurnWorkQueue,
        cancel_rx: &watch::Receiver<bool>,
    ) -> Result<Vec<String>, OuterDocsRefusal> {
        let declared = Self::declared_outer_docs(scope, queue, &self.repo_root);
        if declared.is_empty() {
            return Ok(Vec::new());
        }
        let registry = OuterDocsRegistry::new(&self.config.outer_docs);
        preload_declared_outer_docs(
            &self.state,
            &self.repo_root,
            &registry,
            &declared,
            self.config.context.token_budget,
            cancel_rx,
        )
        .await
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

        // Every attempt starts from a clean transient context: the files a
        // previous step discovered and the documents it mounted describe work
        // that is already finished.
        cleanup_transient_context(&self.state).await;
        preload_declared_files(&self.state, &self.repo_root, &files, &readonly_files).await;
        // A step declares the external specifications it obeys, so the harness
        // mounts them before the turn starts instead of leaving the Coder to
        // discover them through research rounds. A trust refusal ends the step
        // before any model work is spent: only the developer can authorize the
        // source, and a replanning wave cannot widen a declaration that lives
        // outside the repository.
        let outer_docs = match self
            .preload_step_outer_docs(&scope, &queue, &cancel_rx)
            .await
        {
            Ok(diagnostics) => diagnostics,
            Err(refusal) => return StepTurnResult::Abort(refusal.to_message()),
        };

        let plan_context = Self::plan_context(&scope, &queue);
        let work_context = Self::work_context(&scope, &title, queue);
        let mut prompt = prompt;
        append_developer_directives(&mut prompt, instructions.as_deref());
        append_outer_docs_diagnostics(&mut prompt, &outer_docs);

        // The step executor already persisted the lifecycle of the reviewed entity
        // before it asked for this turn, so a review scope is published here: the
        // client observes the finding or the group the run has taken over instead of
        // the `TODO` it held when the batch was launched. A plan scope carries no
        // publication of its own, because the plan supervisor owns it.
        if let ExecutionScope::ReviewGroup { review_id, .. }
        | ExecutionScope::ReviewFinding { review_id, .. } = &scope
        {
            publish_review(&self.state, &self.repo_root, review_id).await;
        }

        let policy = build_apply_policy(&self.state, self.config.repl.workflow);
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
            // The commit that carried the change set is the durable pointer of
            // the completed step, so it travels with the success instead of
            // being read back from the repository afterwards.
            let (summary, commit) = match &turn.result {
                Some(tauqe_protocol::ModelResult::Edit {
                    summary, outcome, ..
                }) => (summary.clone(), outcome.commit_hash.clone()),
                _ => (String::new(), None),
            };
            return StepTurnResult::Success { summary, commit };
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
            // An abort is a diagnosis of the same weight as a blocker: the plan
            // or the group records why the horizon could not be reprojected, and
            // the message carries the host action the developer must perform
            // before the wave can run again.
            StructuredRefineStatus::Aborted(report) => RefinementResult::Blocked {
                reason: report.to_message(),
                tier: synthesized_tier,
            },
            StructuredRefineStatus::Cancelled => RefinementResult::Blocked {
                reason: "The Architect wave was cancelled".to_string(),
                tier: synthesized_tier,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::plan::storage::PlanStorage;
    use tauqe_protocol::model::TurnWorkItem;
    use tauqe_protocol::{Plan, PlanItem};

    /// Ordered queue every work-context assertion advances through.
    fn queue() -> TurnWorkQueue {
        TurnWorkQueue {
            items: vec![
                TurnWorkItem {
                    id: "1".to_string(),
                    title: "Declare the contract".to_string(),
                },
                TurnWorkItem {
                    id: "2".to_string(),
                    title: "Wire the client".to_string(),
                },
            ],
            active_index: 1,
        }
    }

    #[test]
    fn a_plan_scope_declares_a_plan_step_context() {
        let context = ServerImplementationHost::work_context(
            &ExecutionScope::Plan {
                plan_id: "plan-1".to_string(),
            },
            "Plan One",
            queue(),
        )
        .expect("a plan scope declares its work");

        match context {
            TurnWorkContext::PlanStep {
                plan_id,
                plan_title,
                queue,
            } => {
                assert_eq!(plan_id, "plan-1");
                assert_eq!(plan_title, "Plan One");
                assert_eq!(queue.active().map(|item| item.id.as_str()), Some("2"));
            }
            other => panic!("unexpected context: {other:?}"),
        }
    }

    #[test]
    fn a_review_finding_scope_declares_a_review_step_context() {
        let context = ServerImplementationHost::work_context(
            &ExecutionScope::ReviewFinding {
                review_id: "rev-42".to_string(),
                item_id: 7,
            },
            "Guard the parser",
            queue(),
        )
        .expect("a review finding declares its work");

        match context {
            TurnWorkContext::ReviewStep {
                review_id,
                review_title,
                queue,
            } => {
                assert_eq!(review_id, "rev-42");
                // The typed request carries the horizon title, not the session
                // title, so the review is addressed by its identifier.
                assert_eq!(review_title, "rev-42");
                assert_eq!(queue.active().map(|item| item.id.as_str()), Some("2"));
            }
            other => panic!("unexpected context: {other:?}"),
        }
    }

    #[test]
    fn a_review_group_scope_declares_a_review_group_step_context() {
        let context = ServerImplementationHost::work_context(
            &ExecutionScope::ReviewGroup {
                review_id: "rev-42".to_string(),
                group_id: "arch-1".to_string(),
            },
            "Unified error handling",
            queue(),
        )
        .expect("a review group declares its work");

        match context {
            TurnWorkContext::ReviewGroupStep {
                review_id,
                review_title,
                group_id,
                group_title,
                queue,
            } => {
                assert_eq!(review_id, "rev-42");
                assert_eq!(review_title, "rev-42");
                assert_eq!(group_id, "arch-1");
                // The frame the executor resolves carries the group title as the
                // horizon title, so the step names the group it executes.
                assert_eq!(group_title, "Unified error handling");
                assert_eq!(queue.active().map(|item| item.id.as_str()), Some("2"));
            }
            other => panic!("unexpected context: {other:?}"),
        }
    }

    #[test]
    fn the_active_step_declares_the_external_documents_it_preloads() {
        let root = std::env::temp_dir().join(format!(
            "tauqe-host-outer-docs-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create the scratch repository root");
        let plan = Plan::from_parts(
            "outer-docs-plan".to_string(),
            "Outer Docs Plan".to_string(),
            None,
            0,
            0,
            Vec::new(),
            vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Read the published contract".to_string(),
                    // A leaf always declares what it targets, so the horizon
                    // validator accepts it and the step preloads real files.
                    files: vec!["src/client.rs".to_string()],
                    outer_docs: vec!["vendor/api.md".to_string()],
                    ..PlanItem::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Wire the client".to_string(),
                    files: vec!["src/client.rs".to_string()],
                    ..PlanItem::default()
                },
            ],
        );
        PlanStorage::save_plan(&root, &plan).expect("store the plan");
        let scope = ExecutionScope::Plan {
            plan_id: "outer-docs-plan".to_string(),
        };
        let queue = |id: &str, title: &str| TurnWorkQueue {
            items: vec![TurnWorkItem {
                id: id.to_string(),
                title: title.to_string(),
            }],
            active_index: 0,
        };

        assert_eq!(
            ServerImplementationHost::declared_outer_docs(
                &scope,
                &queue("1", "Read the published contract"),
                &root
            ),
            vec!["vendor/api.md".to_string()]
        );
        // The declaration belongs to the running leaf, so a step that declares
        // none mounts nothing even though the horizon declares documentation.
        assert!(ServerImplementationHost::declared_outer_docs(
            &scope,
            &queue("2", "Wire the client"),
            &root
        )
        .is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_scope_that_cannot_be_resolved_declares_no_external_documents() {
        let queue = TurnWorkQueue {
            items: vec![TurnWorkItem {
                id: "1".to_string(),
                title: "Step".to_string(),
            }],
            active_index: 0,
        };
        let scope = ExecutionScope::Plan {
            plan_id: "absent".to_string(),
        };
        let root = std::env::temp_dir().join("tauqe-host-absent-scope");

        assert!(ServerImplementationHost::declared_outer_docs(&scope, &queue, &root).is_empty());
    }
}
