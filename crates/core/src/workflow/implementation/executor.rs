//! Step executor for one implementation horizon.
//!
//! The executor turns a [`BatchStep`] of a running queue into a bounded step
//! turn: it resolves the leaf inside the horizon the [`ExecutionScope`] names,
//! persists the step lifecycle through the storage that owns that horizon, and
//! drives the coder ladder until the step succeeds, blocks, or exhausts the
//! rungs the project configured.
//!
//! Nothing here touches provider telemetry, usage windows, or the context
//! layers: the turn itself belongs to the [`ImplementationHost`], and the
//! request only declares the files the step targets so the host can preload
//! them.

use std::path::Path;

use crate::config::ModelsConfig;
use crate::plan::prompt::{format_plan_step_execution_prompt, format_plan_step_retry_prompt};
use crate::plan::storage::PlanStorage;
use crate::providers::{
    format_session_id, StreamEvent, SESSION_PREFIX_PLAN, SESSION_PREFIX_REVIEW,
};
use crate::review::storage::ReviewStorage;
use crate::workflow::batch::{BatchStep, BatchStepExecutor, BatchStepOutcome};
use crate::workflow::implementation::{
    classify_scope_stall, ExecutionScope, ImplementationFrame, ImplementationHost, StepFailure,
    StepTurnRequest, StepTurnResult,
};
use async_trait::async_trait;
use tauqe_protocol::model::TurnWorkQueue;
use tauqe_protocol::{
    EditProtocolKind, ModelRole, ModelSelection, PlanItem, PlanItemStatus, PlanScopeEvaluation,
    ReasoningEffort, TurnWorkItem,
};
use tokio::sync::{mpsc, watch};

/// Configuration of one implementation run.
///
/// The executor stays independent of where the configuration came from: the
/// coder ladder, the protocol vocabulary, and the scheduled queue arrive here,
/// so one executor serves a plan batch, a review group run, and a degenerate
/// single-finding run alike.
#[derive(Debug, Clone)]
pub struct ImplementationRunConfig {
    /// Coder ladder and the automatic escalation policy of the run.
    pub models: ModelsConfig,
    /// Protocol that shapes the step prompts and their control tags.
    pub protocol: EditProtocolKind,
    /// Turn marker bound to the tagged control channel.
    pub marker: Option<String>,
    /// Coder selection the run starts from.
    pub selection: Option<ModelSelection>,
    /// Additional instructions of the run, forwarded to every step turn.
    pub instructions: Option<String>,
    /// Ordered identifiers of the scheduled leaves, the running one included.
    ///
    /// The queue drives the work marker and the `[i/N]` position a client
    /// renders. A step the queue does not list is appended to it, so the
    /// running leaf is always declared and its position is always exact.
    pub queue: Vec<String>,
}

/// Executes the leaves of one implementation horizon as bounded step turns.
///
/// The horizon is resolved from the authoritative storage on every step, so a
/// status a previous step committed is observed by the next one and the
/// executor never carries a second copy of the step graph.
pub struct PlanImplementationStepExecutor<'a> {
    repo_root: &'a Path,
    scope: ExecutionScope,
    host: &'a dyn ImplementationHost,
    config: ImplementationRunConfig,
}

impl<'a> PlanImplementationStepExecutor<'a> {
    /// Creates an executor for the horizon `scope` resolves to.
    pub fn new(
        repo_root: &'a Path,
        scope: ExecutionScope,
        host: &'a dyn ImplementationHost,
        config: ImplementationRunConfig,
    ) -> Self {
        Self {
            repo_root,
            scope,
            host,
            config,
        }
    }
}

#[async_trait]
impl BatchStepExecutor for PlanImplementationStepExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        _stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        Ok(self.run(step, cancel_rx).await)
    }
}

impl PlanImplementationStepExecutor<'_> {
    /// Runs one leaf of the horizon to its terminal outcome.
    async fn run(&self, step: &BatchStep, cancel_rx: watch::Receiver<bool>) -> BatchStepOutcome {
        // The horizon is re-resolved from the storage that owns it, so the
        // executor never decides a lifecycle transition from a stale copy of
        // the step graph.
        let mut frame = match self.scope.resolve(self.repo_root) {
            Ok(frame) => frame,
            Err(failure) => return BatchStepOutcome::failed(failure.into_reason()),
        };

        let leaf = match frame.plan.find_item(&step.id) {
            Some(item) if item.is_leaf() => item.clone(),
            Some(item) => {
                return BatchStepOutcome::failed(format!(
                    "Step '{}' ({}) is a container of the implementation horizon and never an execution target",
                    item.id, item.title
                ));
            }
            None => {
                return BatchStepOutcome::failed(format!(
                    "Step '{}' is not part of the implementation horizon",
                    step.id
                ));
            }
        };

        match leaf.status {
            PlanItemStatus::Done => return BatchStepOutcome::succeeded(),
            PlanItemStatus::Discussion => {
                return BatchStepOutcome::failed(format!(
                    "Step '{}' ({}) is in DISCUSSION status and cannot be executed until reviewed",
                    leaf.id, leaf.title
                ));
            }
            PlanItemStatus::Blocked => {
                return BatchStepOutcome::failed(format!(
                    "Step '{}' ({}) is BLOCKED; resolve the reported blocker and replan before executing",
                    leaf.id, leaf.title
                ));
            }
            PlanItemStatus::Cancelled => {
                return BatchStepOutcome::failed(format!(
                    "Step '{}' ({}) is CANCELLED and cannot be executed",
                    leaf.id, leaf.title
                ));
            }
            PlanItemStatus::Todo | PlanItemStatus::InProgress => {}
        }

        // A step may run only once every declared dependency is DONE. The queue
        // is assembled from this very rule, so the check guards the run against
        // a status drift between the assembly of the queue and the launch of a
        // step, and a stalled scope is reported through the shared taxonomy.
        let readiness = frame.plan.evaluate_scope(Some(leaf.id.as_str()));
        if !matches!(readiness, PlanScopeEvaluation::Ready(_)) {
            let failure = classify_scope_stall(&frame.plan, Some(leaf.id.as_str()), &readiness);
            return BatchStepOutcome::failed(failure.into_reason());
        }

        // Phase boundary: a cancel observed before the step is launched ends the
        // step before any model turn is requested.
        if *cancel_rx.borrow() {
            return BatchStepOutcome::cancelled();
        }

        if let Err(failure) =
            self.set_status(&mut frame, &leaf.id, PlanItemStatus::InProgress, None)
        {
            return BatchStepOutcome::failed(failure.into_reason());
        }

        tracing::info!(
            scope = ?frame.scope,
            step_id = leaf.id.as_str(),
            "implementation step started"
        );

        let queue = self.work_queue(&frame, &leaf.id);
        let session_id = self.session_id(&frame.scope);
        let models = &self.config.models;
        let marker = self.config.marker.as_deref();

        // The selected coder is the starting rung of the step: an explicit
        // selection runs at its own reasoning depth, and a later escalation
        // continues above it instead of restarting the ladder at its base.
        let mut selection = self
            .config
            .selection
            .clone()
            .unwrap_or(ModelSelection::Role(ModelRole::Coder));
        let mut ladder_idx = models.coder_tier_index(Some(&selection));
        let mut reasoning_effort: Option<ReasoningEffort> =
            models.resolve_selection_effort(ModelRole::Coder, &selection);
        // Content attempts already spent on the current coder rung. The counter
        // resets on promotion, and the budget itself is never cached: it is read
        // from the rung `ladder_idx` names wherever the spent attempts are
        // compared, so a promotion switches to the promoted budget with nothing
        // left to keep in sync.
        let mut tier_attempts = 0u32;

        let mut prompt = match format_plan_step_execution_prompt(
            &frame.plan,
            &leaf.id,
            self.config.protocol,
            marker,
        ) {
            Ok(prompt) => prompt,
            Err(err) => {
                self.reset(&mut frame, &leaf.id);
                return BatchStepOutcome::failed(err);
            }
        };

        loop {
            // Phase boundary: every attempt is a whole bounded turn, so a
            // cancel observed here ends the step before another turn is
            // requested and returns the step to TODO.
            if *cancel_rx.borrow() {
                self.reset(&mut frame, &leaf.id);
                return BatchStepOutcome::cancelled();
            }

            let request = StepTurnRequest {
                scope: frame.scope.clone(),
                title: frame.plan.title.clone(),
                queue: queue.clone(),
                prompt: prompt.clone(),
                files: leaf.files.clone(),
                readonly_files: leaf.readonly_files.clone(),
                selection: selection.clone(),
                reasoning_effort,
                instructions: self.config.instructions.clone(),
                marker: self.config.marker.clone(),
                session_id: Some(session_id.clone()),
                cancel_rx: cancel_rx.clone(),
            };

            let host = self.host;
            match host.run_step(request).await {
                StepTurnResult::Success { summary } => {
                    if let Err(failure) =
                        self.set_status(&mut frame, &leaf.id, PlanItemStatus::Done, None)
                    {
                        return BatchStepOutcome::failed(failure.into_reason());
                    }
                    tracing::info!(
                        step_id = leaf.id.as_str(),
                        summary = summary.as_str(),
                        "implementation step completed"
                    );
                    return BatchStepOutcome::succeeded();
                }
                StepTurnResult::Blocked(reason) => {
                    // The block is only meaningful when the durable record
                    // carries it: a BLOCKED status that never reached disk would
                    // let the next queue schedule the same step again, so a
                    // failed write is reported instead of a block nobody can
                    // observe.
                    if let Err(failure) = self.set_status(
                        &mut frame,
                        &leaf.id,
                        PlanItemStatus::Blocked,
                        Some(reason.as_str()),
                    ) {
                        return BatchStepOutcome::failed(failure.into_reason());
                    }
                    tracing::warn!(
                        step_id = leaf.id.as_str(),
                        reason = reason.as_str(),
                        "implementation step blocked"
                    );
                    return BatchStepOutcome::blocked(reason);
                }
                StepTurnResult::Abort(message) => {
                    self.reset(&mut frame, &leaf.id);
                    tracing::warn!(
                        step_id = leaf.id.as_str(),
                        message = message.as_str(),
                        "implementation step aborted; developer action in the host environment is required"
                    );
                    return BatchStepOutcome::failed(message);
                }
                StepTurnResult::Cancelled => {
                    self.reset(&mut frame, &leaf.id);
                    return BatchStepOutcome::cancelled();
                }
                StepTurnResult::Failed(failure) => {
                    if *cancel_rx.borrow() {
                        self.reset(&mut frame, &leaf.id);
                        return BatchStepOutcome::cancelled();
                    }
                    // A failed change set the host could not roll back leaves
                    // the working tree carrying the bytes of the aborted
                    // attempt. Retrying would build the next attempt on lines
                    // nobody verified, so the failure ends the step here.
                    if failure.is_unreverted() {
                        self.reset(&mut frame, &leaf.id);
                        tracing::warn!(
                            step_id = leaf.id.as_str(),
                            reason = failure.reason(),
                            "implementation step left unreverted changes; the step ends without a retry"
                        );
                        return BatchStepOutcome::failed(failure.into_reason());
                    }
                    // A failure that is not confined to the step never spends a
                    // tier attempt: a stronger model would meet the same broken
                    // transport, a missing credential, or the same cancellation.
                    if !failure.is_domain() {
                        self.reset(&mut frame, &leaf.id);
                        tracing::warn!(
                            step_id = leaf.id.as_str(),
                            reason = failure.reason(),
                            "implementation step ended on an infrastructure failure; no coder tier was spent"
                        );
                        return BatchStepOutcome::failed(failure.into_reason());
                    }

                    tier_attempts += 1;
                    if tier_attempts >= models.coder_tier(ladder_idx).attempts() {
                        // The rung automatic escalation would enter next. `None`
                        // means the ladder is already saturated: the step sits
                        // on the strongest eligible rung, or the developer
                        // disabled automatic escalation for the run.
                        let next_rung = if models.auto_level_up {
                            models.next_coder_tier(ladder_idx)
                        } else {
                            None
                        };
                        match next_rung {
                            None => {
                                self.reset(&mut frame, &leaf.id);
                                let message = if models.auto_level_up {
                                    saturation_message(models, &selection, &leaf, failure.reason())
                                } else {
                                    failure.reason().to_string()
                                };
                                tracing::warn!(
                                    step_id = leaf.id.as_str(),
                                    tier = ladder_idx,
                                    reason = failure.reason(),
                                    "implementation step escalation is exhausted"
                                );
                                return BatchStepOutcome::failed(message);
                            }
                            Some((next_idx, tier)) => {
                                tracing::warn!(
                                    step_id = leaf.id.as_str(),
                                    from_tier = ladder_idx,
                                    to_tier = next_idx,
                                    to_model = %tier.model,
                                    effort = ?tier.effort,
                                    reason = failure.reason(),
                                    "implementation step coder escalation"
                                );
                                ladder_idx = next_idx;
                                selection = ModelSelection::Specific(tier.model);
                                reasoning_effort = tier.effort;
                                tier_attempts = 0;
                            }
                        }
                    } else {
                        tracing::warn!(
                            step_id = leaf.id.as_str(),
                            tier = ladder_idx,
                            tier_attempt = tier_attempts,
                            max_tier_attempts = models.coder_tier(ladder_idx).attempts(),
                            reason = failure.reason(),
                            "implementation step retries the same coder tier"
                        );
                    }

                    prompt = format_plan_step_retry_prompt(
                        "",
                        Some(failure.reason()),
                        self.config.protocol,
                        marker,
                    );
                }
            }
        }
    }

    /// Renders the ordered queue of the run with the running position.
    fn work_queue(&self, frame: &ImplementationFrame, step_id: &str) -> TurnWorkQueue {
        let mut scheduled = self.config.queue.clone();
        let active_index = match scheduled.iter().position(|id| id.as_str() == step_id) {
            Some(index) => index,
            None => {
                scheduled.push(step_id.to_string());
                scheduled.len().saturating_sub(1)
            }
        };
        let items = scheduled
            .iter()
            .map(|id| match frame.plan.find_item(id.as_str()) {
                Some(member) => TurnWorkItem {
                    id: member.id.clone(),
                    title: member.title.clone(),
                },
                // A scheduled identifier the horizon no longer carries keeps
                // the positions aligned: it stands in for the missing title
                // instead of silently dropping a member of the queue.
                None => TurnWorkItem {
                    id: id.clone(),
                    title: id.clone(),
                },
            })
            .collect();
        TurnWorkQueue {
            items,
            active_index,
        }
    }

    /// Derives the provider session identifier shared by every step of the run.
    ///
    /// Consecutive turns of one horizon therefore extend the same provider
    /// prefix cache instead of warming a new one per step.
    fn session_id(&self, scope: &ExecutionScope) -> String {
        match scope {
            ExecutionScope::Plan { plan_id } => format_session_id(SESSION_PREFIX_PLAN, plan_id),
            ExecutionScope::ReviewGroup { review_id, .. }
            | ExecutionScope::ReviewFinding { review_id, .. } => {
                format_session_id(SESSION_PREFIX_REVIEW, review_id)
            }
        }
    }

    /// Persists one step status through the storage that owns the horizon.
    ///
    /// The horizon is the single source of truth for its steps, so a lifecycle
    /// transition never lives in the executor alone: a plan step is written by
    /// [`PlanStorage::update_item`], a review group step is written as the whole
    /// horizon of that group by [`ReviewStorage::update_group_subplan`], and a
    /// degenerate single-finding run writes nothing, because the statuses of the
    /// findings it belongs to stay frozen while it executes.
    ///
    /// Every storage entry point offloads its own file I/O through the blocking
    /// pool, so the executor awaits the step loop without blocking a worker.
    fn set_status(
        &self,
        frame: &mut ImplementationFrame,
        step_id: &str,
        status: PlanItemStatus,
        blocked_reason: Option<&str>,
    ) -> Result<(), StepFailure> {
        match frame.scope.clone() {
            ExecutionScope::Plan { plan_id } => {
                let updated = PlanStorage::update_item(
                    self.repo_root,
                    &plan_id,
                    step_id,
                    Some(status),
                    blocked_reason,
                )
                .map_err(|err| {
                    StepFailure::infrastructure(format!(
                        "Failed to persist the status of step '{}': {}",
                        step_id, err
                    ))
                })?;
                let Some(plan) = updated else {
                    return Err(StepFailure::infrastructure(format!(
                        "Plan '{}' does not store step '{}'",
                        plan_id, step_id
                    )));
                };
                frame.plan = plan;
                Ok(())
            }
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => {
                let mut items = frame.plan.implementation.items.clone();
                if !set_leaf_state(&mut items, step_id, status, blocked_reason) {
                    return Err(StepFailure::infrastructure(format!(
                        "Review group '{}' of session '{}' does not store step '{}'",
                        group_id, review_id, step_id
                    )));
                }
                let updated = ReviewStorage::update_group_subplan(
                    self.repo_root,
                    &review_id,
                    &group_id,
                    items,
                )
                .map_err(|err| {
                    StepFailure::infrastructure(format!(
                        "Failed to persist the status of step '{}': {}",
                        step_id, err
                    ))
                })?;
                let Some(group) = updated else {
                    return Err(StepFailure::infrastructure(format!(
                        "Review group '{}' of session '{}' is not stored",
                        group_id, review_id
                    )));
                };
                frame.plan.implementation = group.implementation;
                Ok(())
            }
            // A single finding executes as a degenerate in-memory horizon: its
            // finding statuses stay frozen for the whole run, and the run closes
            // the finding through the group completion transition instead.
            ExecutionScope::ReviewFinding { .. } => Ok(()),
        }
    }

    /// Returns a step to TODO after a turn that produced no durable progress.
    fn reset(&self, frame: &mut ImplementationFrame, step_id: &str) {
        if let Err(failure) = self.set_status(frame, step_id, PlanItemStatus::Todo, None) {
            tracing::warn!(
                step_id = step_id,
                error = failure.reason(),
                "failed to return an implementation step to TODO"
            );
        }
    }
}

/// Sets the status and the blocker report of one leaf inside a horizon tree.
///
/// The traversal follows the protocol identity rule: identifiers compare
/// case-insensitively after trimming, so a step is reached by the same spelling
/// the horizon resolves. A container is never a target, because only a leaf
/// carries an authoritative status.
fn set_leaf_state(
    items: &mut [PlanItem],
    step_id: &str,
    status: PlanItemStatus,
    blocked_reason: Option<&str>,
) -> bool {
    for item in items {
        if item.id.trim().eq_ignore_ascii_case(step_id.trim()) {
            if !item.is_leaf() {
                return false;
            }
            item.status = status;
            // The report belongs to the BLOCKED state alone: leaving it drops the
            // stale diagnostic, so a resumed step never keeps describing a
            // blocker that no longer holds.
            item.blocked_reason = if status == PlanItemStatus::Blocked {
                blocked_reason
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .map(str::to_string)
            } else {
                None
            };
            return true;
        }
        if set_leaf_state(&mut item.children, step_id, status, blocked_reason) {
            return true;
        }
    }
    false
}

/// Diagnostic reported when automatic escalation cannot advance beyond the rung
/// a step runs on.
///
/// The verdict reads the typed exclusion of the coder role, never the model
/// name: a run started manually on a model the ladder excludes is already
/// saturated, and so is a run sitting on the strongest eligible rung.
fn saturation_message(
    models: &ModelsConfig,
    selection: &ModelSelection,
    leaf: &PlanItem,
    reason: &str,
) -> String {
    let model = models.resolve_model(selection);
    let saturation = if models.excluded_from_escalation(ModelRole::Coder, &model) {
        format!(
            "`{}` is excluded from the automatic ladder (`level_up = false`)",
            model
        )
    } else {
        format!(
            "`{}` is already the strongest rung of the automatic ladder",
            model
        )
    };
    format!(
        "Coder escalation is exhausted for step '{}' ({}): {}. The failure is terminal, so no stronger coder tier is attempted and the step is not retried.\n\nCause: {}",
        leaf.id, leaf.title, saturation, reason
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    use crate::config::ModelsConfig;
    use crate::plan::storage::PlanStorage;
    use crate::review::storage::ReviewStorage;
    use crate::workflow::batch::{BatchStep, BatchStepExecutor, BatchStepStatus};
    use crate::workflow::implementation::{
        ExecutionScope, ImplementationHost, ImplementationRunConfig,
        PlanImplementationStepExecutor, RefinementRequest, RefinementResult, StepFailure,
        StepTurnRequest, StepTurnResult,
    };
    use async_trait::async_trait;
    use tauqe_protocol::review::{IssueStatus, IssueType, ReviewIssue};
    use tauqe_protocol::{
        ArchitectTier, EditProtocolKind, ModelEntry, ModelRef, ModelSelection, Plan,
        PlanImplementation, PlanItem, PlanItemStatus, ReviewGroup, ReviewItem, ReviewSession,
    };
    use tempfile::tempdir;
    use tokio::sync::{mpsc, watch};

    /// Host that replays a scripted sequence of step results and records every
    /// request it was asked to run.
    struct ScriptedHost {
        script: Mutex<VecDeque<StepTurnResult>>,
        requests: Mutex<Vec<StepTurnRequest>>,
    }

    impl ScriptedHost {
        fn new(script: Vec<StepTurnResult>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> MutexGuard<'_, Vec<StepTurnRequest>> {
            self.requests.lock().unwrap()
        }
    }

    #[async_trait]
    impl ImplementationHost for ScriptedHost {
        async fn run_step(&self, request: StepTurnRequest) -> StepTurnResult {
            self.requests.lock().unwrap().push(request);
            self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                StepTurnResult::Failed(StepFailure::infrastructure("no scripted step result"))
            })
        }

        async fn run_refinement(&self, _request: RefinementRequest) -> RefinementResult {
            RefinementResult::Blocked {
                reason: "the scripted host never refines a horizon".to_string(),
                tier: ArchitectTier::from(ModelRef::openrouter("architect/model")),
            }
        }
    }

    /// Horizon holding one leaf that targets the seam and reads the batch
    /// contract.
    fn horizon_plan() -> Plan {
        Plan::from_parts(
            "step-horizon".to_string(),
            "Step Horizon".to_string(),
            Some("Drive one step through the seam.".to_string()),
            0,
            0,
            Vec::new(),
            vec![PlanItem {
                id: "1".to_string(),
                title: "Implement the seam".to_string(),
                files: vec!["crates/core/src/workflow/implementation.rs".to_string()],
                readonly_files: vec!["crates/core/src/workflow/batch.rs".to_string()],
                ..PlanItem::default()
            }],
        )
    }

    /// Stores a session holding one architectural group horizon and its finding.
    fn stored_review(repo_root: &Path) {
        let group = ReviewGroup {
            id: "arch-1".to_string(),
            title: "Unified seam".to_string(),
            description: Some("Every runner invents its own step loop.".to_string()),
            implementation: PlanImplementation {
                items: vec![PlanItem {
                    id: "s-1".to_string(),
                    title: "Introduce the seam".to_string(),
                    files: vec!["crates/core/src/workflow/implementation.rs".to_string()],
                    ..PlanItem::default()
                }],
                ..PlanImplementation::default()
            },
            ..ReviewGroup::default()
        };
        let finding = ReviewItem::from(ReviewIssue {
            id: 7,
            title: "Guard the executor".to_string(),
            uuid: None,
            group: Some("arch-1".to_string()),
            model: None,
            issue_type: IssueType::Bug,
            status: IssueStatus::Todo,
            description: "The executor duplicates the coder ladder.".to_string(),
            recommendations: Vec::new(),
            files: vec!["crates/core/src/workflow/implementation/executor.rs".to_string()],
            readonly_files: Vec::new(),
            note: None,
        });
        let session = ReviewSession {
            id: "review-horizon".to_string(),
            title: "Horizon audit".to_string(),
            items: vec![finding],
            groups: vec![group],
            ..ReviewSession::default()
        };
        ReviewStorage::save_session(repo_root, &session).unwrap();
    }

    /// Run configuration whose single rung resolves the coder role.
    fn run_config(step_id: &str) -> ImplementationRunConfig {
        ImplementationRunConfig {
            models: ModelsConfig {
                primary: vec![ModelEntry::from(ModelRef::openrouter("base/model"))],
                coder: Some(vec![ModelEntry::from(ModelRef::openrouter("base/model"))]),
                auto_level_up: true,
                ..ModelsConfig::default()
            },
            protocol: EditProtocolKind::Xml,
            marker: None,
            selection: None,
            instructions: None,
            queue: vec![step_id.to_string()],
        }
    }

    /// Coder ladder whose rungs each grant a single content attempt.
    fn two_rung_models() -> ModelsConfig {
        ModelsConfig {
            primary: vec![ModelEntry::from(ModelRef::openrouter("base/model"))],
            coder: Some(vec![
                ModelEntry {
                    model: ModelRef::openrouter("base/model"),
                    effort: None,
                    attempts: Some(1),
                    level_up: None,
                },
                ModelEntry {
                    model: ModelRef::openrouter("stronger/model"),
                    effort: None,
                    attempts: Some(1),
                    level_up: None,
                },
            ]),
            auto_level_up: true,
            ..ModelsConfig::default()
        }
    }

    fn channels() -> (
        mpsc::Sender<crate::providers::StreamEvent>,
        watch::Receiver<bool>,
    ) {
        let (stream_tx, _stream_rx) = mpsc::channel(8);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        (stream_tx, cancel_rx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plan_step_is_marked_in_progress_and_done_through_the_plan_storage() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![StepTurnResult::Success {
            summary: "the seam is in place".to_string(),
        }]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
        };
        let mut executor =
            PlanImplementationStepExecutor::new(dir.path(), scope, &host, run_config("1"));
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor
            .execute_step(&BatchStep::new("1", "seam"), stream_tx, cancel_rx)
            .await
            .unwrap();

        assert!(outcome.is_success());
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Done);

        let requests = host.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].files,
            vec!["crates/core/src/workflow/implementation.rs".to_string()]
        );
        assert_eq!(
            requests[0].readonly_files,
            vec!["crates/core/src/workflow/batch.rs".to_string()]
        );
        assert!(requests[0].prompt.contains("Target Plan Step Execution"));
        assert_eq!(requests[0].queue.active_index, 0);
        assert_eq!(requests[0].queue.items.len(), 1);
        assert_eq!(
            requests[0].session_id.as_deref(),
            Some("tauqe-plan-step-horizon")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocked_step_records_its_report_through_the_plan_storage() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![StepTurnResult::Blocked(
            "the approved data structure cannot express the invariant".to_string(),
        )]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
        };
        let mut executor =
            PlanImplementationStepExecutor::new(dir.path(), scope, &host, run_config("1"));
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor
            .execute_step(&BatchStep::new("1", "seam"), stream_tx, cancel_rx)
            .await
            .unwrap();

        assert_eq!(outcome.status, BatchStepStatus::Blocked);
        assert_eq!(
            outcome.blocked_reason.as_deref(),
            Some("the approved data structure cannot express the invariant")
        );
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        let step = stored.find_item("1").unwrap();
        assert_eq!(step.status, PlanItemStatus::Blocked);
        assert_eq!(
            step.blocked_reason.as_deref(),
            Some("the approved data structure cannot express the invariant")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_domain_failure_climbs_the_coder_ladder_after_the_tier_budget() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
            StepTurnResult::Success {
                summary: "the seam is in place".to_string(),
            },
        ]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
        };
        let mut config = run_config("1");
        config.models = two_rung_models();
        let mut executor = PlanImplementationStepExecutor::new(dir.path(), scope, &host, config);
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor
            .execute_step(&BatchStep::new("1", "seam"), stream_tx, cancel_rx)
            .await
            .unwrap();

        assert!(outcome.is_success());
        let requests = host.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].selection,
            ModelSelection::Specific(ModelRef::openrouter("stronger/model"))
        );
        assert!(requests[1].prompt.contains("Correction Required"));
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Done);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exhausted_ladder_ends_the_step_without_a_retry() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![StepTurnResult::Failed(StepFailure::domain(
            "the patch did not apply",
        ))]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
        };
        let mut config = run_config("1");
        config.models = two_rung_models();
        let mut executor = PlanImplementationStepExecutor::new(dir.path(), scope, &host, config);
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor
            .execute_step(&BatchStep::new("1", "seam"), stream_tx, cancel_rx)
            .await
            .unwrap();

        // One rung, one attempt each: the second attempt runs on the promoted
        // rung and its own single attempt exhausts the ladder.
        assert_eq!(host.requests().len(), 2);
        assert_eq!(outcome.status, BatchStepStatus::Failed);
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Todo);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_review_group_step_is_written_through_the_session_storage() {
        let dir = tempdir().unwrap();
        stored_review(dir.path());
        let host = ScriptedHost::new(vec![StepTurnResult::Success {
            summary: "the horizon advanced".to_string(),
        }]);
        let scope = ExecutionScope::ReviewGroup {
            review_id: "review-horizon".to_string(),
            group_id: "arch-1".to_string(),
        };
        let mut executor =
            PlanImplementationStepExecutor::new(dir.path(), scope, &host, run_config("s-1"));
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor
            .execute_step(&BatchStep::new("s-1", "seam"), stream_tx, cancel_rx)
            .await
            .unwrap();

        assert!(outcome.is_success());
        let stored = ReviewStorage::load_review(dir.path(), "review-horizon")
            .unwrap()
            .unwrap();
        let group = stored
            .groups
            .iter()
            .find(|group| group.id == "arch-1")
            .expect("the group is stored");
        assert_eq!(group.implementation.items[0].status, PlanItemStatus::Done);
        // The finding statuses stay frozen while the horizon runs.
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_finding_run_never_writes_the_session() {
        let dir = tempdir().unwrap();
        stored_review(dir.path());
        let host = ScriptedHost::new(vec![StepTurnResult::Success {
            summary: "the finding is addressed".to_string(),
        }]);
        let scope = ExecutionScope::ReviewFinding {
            review_id: "review-horizon".to_string(),
            item_id: 7,
        };
        let mut executor =
            PlanImplementationStepExecutor::new(dir.path(), scope, &host, run_config("7"));
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor
            .execute_step(&BatchStep::new("7", "finding"), stream_tx, cancel_rx)
            .await
            .unwrap();

        assert!(outcome.is_success());
        let stored = ReviewStorage::load_review(dir.path(), "review-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
        let requests = host.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].prompt.contains("Target Plan Step Execution"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreverted_failure_is_terminal_and_returns_the_step_to_todo() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![StepTurnResult::Failed(StepFailure::unreverted(
            "working tree is dirty",
        ))]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
        };
        let mut executor =
            PlanImplementationStepExecutor::new(dir.path(), scope, &host, run_config("1"));
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor
            .execute_step(&BatchStep::new("1", "seam"), stream_tx, cancel_rx)
            .await
            .unwrap();

        assert_eq!(outcome.status, BatchStepStatus::Failed);
        assert_eq!(host.requests().len(), 1);
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Todo);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_infrastructure_failure_is_terminal_without_spending_a_tier() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![StepTurnResult::Failed(StepFailure::infrastructure(
            "the provider stream broke",
        ))]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
        };
        let mut config = run_config("1");
        config.models = two_rung_models();
        let mut executor = PlanImplementationStepExecutor::new(dir.path(), scope, &host, config);
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor
            .execute_step(&BatchStep::new("1", "seam"), stream_tx, cancel_rx)
            .await
            .unwrap();

        assert_eq!(outcome.status, BatchStepStatus::Failed);
        // A transport failure is neutral to the tier budget, so nothing escalates.
        assert_eq!(host.requests().len(), 1);
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Todo);
    }
}
