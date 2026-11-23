//! Resolve an addressed leaf and drive its bounded coder attempts.

use async_trait::async_trait;
use tauqe_core::config::ModelsConfig;
use tauqe_core::models::{ModelEntry, ModelRole, ModelSelection, ReasoningEffort};
use tauqe_core::plan::prompt::{format_plan_step_execution_prompt, format_plan_step_retry_prompt};
use tauqe_core::plan::{PlanImplementation, PlanItem, PlanItemStatus, PlanScopeEvaluation};
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::implementation::{
    classify_scope_stall, ImplementationFrame, StepFailure, StepTurnRequest, StepTurnResult,
    StepTurnSettlement,
};
use tauqe_core::workflow::turn::{TurnWorkItem, TurnWorkQueue};
use tauqe_core::workflow::EditProtocolKind;
use tokio::sync::{mpsc, watch};

/// Configuration of one implementation run.
///
/// The executor stays independent of where the configuration came from: the
/// coder ladder, the protocol vocabulary, and the scheduled queue arrive here,
/// so one executor serves a plan batch, a review group run, and a degenerate
/// single-finding run alike.
#[derive(Debug, Clone)]
pub(crate) struct ImplementationRunConfig {
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

/// Operations supplied by the scenario, independent of the document backend.
#[async_trait]
pub(crate) trait LeafOperations: Send + Sync {
    async fn read(&self) -> Result<ImplementationFrame, StepFailure>;
    fn session_id(&self) -> String;
    async fn begin(&self, step_id: &str) -> Result<Option<PlanImplementation>, StepFailure>;
    /// Settle an admitted leaf before the queue observes its verdict.
    async fn finish(&self, step_id: &str, outcome: &LeafOutcome) -> Result<(), StepFailure>;
    /// Runs a turn and returns any previous-answer text needed for a retry.
    async fn attempt(&self, request: StepTurnRequest) -> (StepTurnSettlement, String);
}

/// A leaf verdict is independent of both the batch policy and the disk receipt.
pub(crate) enum LeafOutcome {
    AlreadyDone,
    Completed(StepTurnSettlement),
    Refused(String),
    Blocked(String, StepTurnSettlement),
    Exhausted(String, StepTurnSettlement),
    /// The attempt diagnostic includes the developer's recommended action.
    Aborted(String),
    Infrastructure(StepFailure),
    Unreverted(StepFailure),
    Cancelled(Option<StepTurnSettlement>),
    TransitionFailed {
        failure: StepFailure,
        attempt: Option<StepTurnSettlement>,
    },
}

pub(crate) struct LeafExecutor<'a> {
    pub operations: &'a dyn LeafOperations,
    pub config: &'a ImplementationRunConfig,
}

impl LeafExecutor<'_> {
    /// Runs one leaf of the horizon to its terminal outcome.
    pub(crate) async fn run(
        &self,
        step_id: &str,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> LeafOutcome {
        // The horizon is re-resolved from the storage that owns it, so the
        // executor never decides a lifecycle transition from a stale copy of
        // the step graph.
        let mut frame = match self.operations.read().await {
            Ok(frame) => frame,
            Err(failure) => return LeafOutcome::Infrastructure(failure),
        };

        let leaf = match frame.plan.implementation.find_item(step_id) {
            Some(item) if item.is_leaf() => item.clone(),
            Some(item) => {
                return LeafOutcome::Refused(format!(
                    "Step '{}' ({}) is a container of the implementation horizon and never an execution target",
                    item.id, item.title
                ));
            }
            None => {
                return LeafOutcome::Refused(format!(
                    "Step '{}' is not part of the implementation horizon",
                    step_id
                ));
            }
        };

        match leaf.status {
            PlanItemStatus::Done => return LeafOutcome::AlreadyDone,
            PlanItemStatus::Discussion => {
                return LeafOutcome::Refused(format!(
                    "Step '{}' ({}) is in DISCUSSION status and cannot be executed until reviewed",
                    leaf.id, leaf.title
                ));
            }
            PlanItemStatus::Blocked => {
                return LeafOutcome::Refused(format!(
                    "Step '{}' ({}) is BLOCKED; resolve the reported blocker and replan before executing",
                    leaf.id, leaf.title
                ));
            }
            PlanItemStatus::Cancelled => {
                return LeafOutcome::Refused(format!(
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
        let readiness = frame
            .plan
            .implementation
            .evaluate_scope(Some(leaf.id.as_str()));
        if !matches!(readiness, PlanScopeEvaluation::Ready(_)) {
            let failure = classify_scope_stall(&frame.plan, Some(leaf.id.as_str()), &readiness);
            return LeafOutcome::Refused(failure.into_reason());
        }

        // Phase boundary: a cancel observed before the step is launched ends the
        // step before any model turn is requested.
        if *cancel_rx.borrow() {
            return LeafOutcome::Cancelled(None);
        }

        match self.operations.begin(&leaf.id).await {
            Ok(Some(implementation)) => frame.plan.implementation = implementation,
            Ok(None) => {}
            Err(failure) => {
                return LeafOutcome::TransitionFailed {
                    failure,
                    attempt: None,
                };
            }
        }

        tracing::info!(
            scope = ?frame.scope,
            step_id = leaf.id.as_str(),
            "implementation step started"
        );

        let outcome = self.run_attempts(&frame, &leaf, stream_tx, cancel_rx).await;
        if let Err(failure) = self.operations.finish(&leaf.id, &outcome).await {
            return LeafOutcome::TransitionFailed {
                failure,
                attempt: match outcome {
                    LeafOutcome::Completed(receipt)
                    | LeafOutcome::Blocked(_, receipt)
                    | LeafOutcome::Exhausted(_, receipt) => Some(receipt),
                    LeafOutcome::Cancelled(receipt) => receipt,
                    _ => None,
                },
            };
        }
        match &outcome {
            LeafOutcome::Completed(receipt) => {
                if let StepTurnResult::Success { summary, .. } = &receipt.result {
                    tracing::info!(
                        step_id = leaf.id.as_str(),
                        summary = summary.as_str(),
                        "implementation step completed"
                    );
                }
            }
            LeafOutcome::Blocked(reason, _) => {
                tracing::warn!(
                    step_id = leaf.id.as_str(),
                    reason = reason.as_str(),
                    "implementation step blocked"
                );
            }
            _ => {}
        }
        outcome
    }

    async fn run_attempts(
        &self,
        frame: &ImplementationFrame,
        leaf: &PlanItem,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> LeafOutcome {
        let queue = self.work_queue(frame, &leaf.id);
        let session_id = self.operations.session_id();
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
        // resets on promotion, and `max_tier_attempts` carries the budget it is
        // compared against. The budget comes from the selected entry of the
        // role catalog rather than from the escalation ladder: a model the role
        // excludes from escalation (`level_up = false`) occupies no ladder
        // position, so reading the ladder would charge it the budget of an
        // unrelated rung.
        let mut tier_attempts = 0u32;
        let mut max_tier_attempts = models.resolve_selection_attempts(ModelRole::Coder, &selection);

        let base_prompt = match format_plan_step_execution_prompt(
            &frame.plan,
            &leaf.id,
            self.config.protocol,
            marker,
        ) {
            Ok(prompt) => prompt,
            Err(err) => return LeafOutcome::Refused(err),
        };
        let mut prompt = base_prompt.clone();

        loop {
            // Phase boundary: every attempt is a whole bounded turn, so a
            // cancel observed here ends the step before another turn is requested.
            if *cancel_rx.borrow() {
                return LeafOutcome::Cancelled(None);
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

            let (settlement, answer) = self.operations.attempt(request).await;
            match &settlement.result {
                StepTurnResult::Success { .. } => return LeafOutcome::Completed(settlement),
                StepTurnResult::Blocked(reason) => {
                    return LeafOutcome::Blocked(reason.clone(), settlement);
                }
                StepTurnResult::Abort(message) => {
                    tracing::warn!(
                        step_id = leaf.id.as_str(),
                        message = message.as_str(),
                        "implementation step aborted; developer action in the host environment is required"
                    );
                    return LeafOutcome::Aborted(message.clone());
                }
                StepTurnResult::Cancelled => {
                    return LeafOutcome::Cancelled(Some(settlement));
                }
                StepTurnResult::Failed(failure) => {
                    if *cancel_rx.borrow() {
                        return LeafOutcome::Cancelled(Some(settlement));
                    }
                    // A failed change set the host could not roll back leaves
                    // the working tree carrying the bytes of the aborted
                    // attempt. Retrying would build the next attempt on lines
                    // nobody verified, so the failure ends the step here.
                    if failure.is_unreverted() {
                        tracing::warn!(
                            step_id = leaf.id.as_str(),
                            reason = failure.reason(),
                            "implementation step left unreverted changes; the step ends without a retry"
                        );
                        return LeafOutcome::Unreverted(failure.clone());
                    }
                    // A failure that is not confined to the step never spends a
                    // tier attempt: a stronger model would meet the same broken
                    // transport, a missing credential, or the same cancellation.
                    if !failure.is_domain() {
                        tracing::warn!(
                            step_id = leaf.id.as_str(),
                            reason = failure.reason(),
                            "implementation step ended on an infrastructure failure; no coder tier was spent"
                        );
                        return LeafOutcome::Infrastructure(failure.clone());
                    }

                    tier_attempts += 1;
                    if tier_attempts >= max_tier_attempts {
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
                                let message = if models.auto_level_up {
                                    saturation_message(models, &selection, leaf, failure.reason())
                                } else {
                                    failure.reason().to_string()
                                };
                                tracing::warn!(
                                    step_id = leaf.id.as_str(),
                                    tier = ladder_idx,
                                    reason = failure.reason(),
                                    "implementation step escalation is exhausted"
                                );
                                return LeafOutcome::Exhausted(message, settlement);
                            }
                            Some((next_idx, tier)) => {
                                tracing::warn!(
                                    step_id = leaf.id.as_str(),
                                    from_tier = ladder_idx,
                                    to_tier = next_idx,
                                    to_model = %tier.model,
                                    effort = ?tier.effort,
                                    attempts = tier.attempts(),
                                    reason = failure.reason(),
                                    "implementation step coder escalation"
                                );
                                ladder_idx = next_idx;
                                max_tier_attempts = tier.attempts();
                                // The promoted rung travels as the whole
                                // `(model, effort)` pair, so no consumer can
                                // collapse the step onto the first entry of the
                                // same model: a rung that only deepens the
                                // reasoning depth keeps its own depth.
                                selection = ModelSelection::Tier {
                                    model: tier.model.clone(),
                                    effort: tier.effort,
                                };
                                reasoning_effort = tier.effort;
                                tier_attempts = 0;
                                // The promotion is a semantic event of the run,
                                // so it reaches the client through the batch
                                // channel instead of staying a log line. A
                                // channel nobody holds any more is not a
                                // failure of the step.
                                let _ = stream_tx
                                    .send(StreamEvent::TierEscalated {
                                        tier: next_idx,
                                        model: tier.model,
                                        role: ModelRole::Coder,
                                        effort: tier.effort,
                                    })
                                    .await;
                            }
                        }
                    } else {
                        tracing::warn!(
                            step_id = leaf.id.as_str(),
                            tier = ladder_idx,
                            tier_attempt = tier_attempts,
                            max_tier_attempts = max_tier_attempts,
                            reason = failure.reason(),
                            "implementation step retries the same coder tier"
                        );
                    }

                    prompt = format_plan_step_retry_prompt(
                        &base_prompt,
                        &answer,
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
            .map(
                |id| match frame.plan.implementation.find_item(id.as_str()) {
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
                },
            )
            .collect();
        TurnWorkQueue {
            items,
            active_index,
        }
    }
}

/// Diagnostic reported when automatic escalation cannot advance beyond the rung
/// a step runs on.
///
/// The verdict reads the typed exclusion of the coder role, never the model
/// name: a run started manually on a rung the ladder excludes is already
/// saturated, and so is a run sitting on the strongest eligible rung.
fn saturation_message(
    models: &ModelsConfig,
    selection: &ModelSelection,
    leaf: &PlanItem,
    reason: &str,
) -> String {
    let model = models.resolve_model(selection);
    // The marker belongs to one rung, so the selected reasoning depth is resolved
    // before the exclusion is read: a sibling rung of the same model at another
    // depth keeps participating in the ladder.
    let effort = models.resolve_selection_effort(ModelRole::Coder, selection);
    let excluded = models.excluded_from_escalation(ModelRole::Coder, &model, effort);
    let rung = ModelEntry::new(model, effort).display_label();
    let saturation = if excluded {
        format!(
            "`{}` is excluded from the automatic ladder (`level_up = false`)",
            rung
        )
    } else {
        format!(
            "`{}` is already the strongest rung of the automatic ladder",
            rung
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

    use super::{ImplementationRunConfig, LeafExecutor, LeafOperations, LeafOutcome};
    use crate::api::Application;
    use crate::workflows::plan::PlanLeaf;
    use crate::workflows::review::sequence::{ReviewLeafAddress, ReviewLeafTarget};
    use crate::workflows::step::queue::{run_queue, QueueStop};
    use async_trait::async_trait;
    use tauqe_core::config::ModelsConfig;
    use tauqe_core::models::{ModelEntry, ModelRef, ModelRole, ModelSelection, ReasoningEffort};
    use tauqe_core::plan::storage::PlanStorage;
    use tauqe_core::plan::{Plan, PlanImplementation, PlanItem, PlanItemStatus};
    use tauqe_core::providers::StreamEvent;
    use tauqe_core::review::storage::ReviewStorage;
    use tauqe_core::review::{IssueStatus, IssueType, ReviewIssue};
    use tauqe_core::review::{ReviewGroup, ReviewItem, ReviewSession};
    use tauqe_core::workflow::implementation::{
        ExecutionScope, ImplementationFrame, StepFailure, StepTurnRequest, StepTurnResult,
        StepTurnSettlement,
    };
    use tauqe_core::workflow::EditProtocolKind;
    use tempfile::tempdir;
    use tokio::sync::{mpsc, watch};

    enum FixtureOperations<'a> {
        Plan(PlanLeaf, &'a ScriptedHost),
        Review(ReviewLeafTarget, &'a ScriptedHost),
    }

    struct FixtureExecutor<'a> {
        operations: FixtureOperations<'a>,
        config: ImplementationRunConfig,
    }

    #[async_trait]
    impl LeafOperations for FixtureOperations<'_> {
        async fn read(&self) -> Result<ImplementationFrame, StepFailure> {
            match self {
                Self::Plan(plan, _) => plan.read().await,
                Self::Review(target, _) => target.read().await,
            }
        }

        fn session_id(&self) -> String {
            match self {
                Self::Plan(plan, _) => plan.session_id(),
                Self::Review(target, _) => target.session_id(),
            }
        }

        async fn begin(&self, id: &str) -> Result<Option<PlanImplementation>, StepFailure> {
            match self {
                Self::Plan(plan, _) => plan.begin(id).await,
                Self::Review(target, _) => target.begin(id).await,
            }
        }

        async fn finish(&self, id: &str, outcome: &LeafOutcome) -> Result<(), StepFailure> {
            match self {
                Self::Plan(plan, _) => plan.finish(id, outcome).await,
                Self::Review(target, _) => target.finish(id, outcome).await,
            }
        }

        async fn attempt(&self, request: StepTurnRequest) -> (StepTurnSettlement, String) {
            match self {
                Self::Plan(_, host) | Self::Review(_, host) => {
                    (host.run_step(request), String::new())
                }
            }
        }
    }

    impl FixtureExecutor<'_> {
        fn leaf_executor(&self) -> LeafExecutor<'_> {
            LeafExecutor {
                operations: &self.operations,
                config: &self.config,
            }
        }

        async fn execute_leaf(
            &self,
            step_id: &str,
            stream_tx: mpsc::Sender<StreamEvent>,
            cancel_rx: watch::Receiver<bool>,
        ) -> LeafOutcome {
            self.leaf_executor()
                .run(step_id, stream_tx, cancel_rx)
                .await
        }
    }

    async fn execute_queue(
        executor: &FixtureExecutor<'_>,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> QueueStop {
        let (_stop_tx, stop_rx) = watch::channel(false);
        run_queue(executor.leaf_executor(), &stream_tx, &cancel_rx, &stop_rx)
            .await
            .stop
    }

    /// Host that replays a scripted sequence of step results and records every
    /// request it was asked to run.
    struct ScriptedHost {
        application: Application,
        script: Mutex<VecDeque<StepTurnSettlement>>,
        requests: Mutex<Vec<StepTurnRequest>>,
    }

    impl ScriptedHost {
        fn new(script: Vec<StepTurnResult>) -> Self {
            Self::with_settlements(script.into_iter().map(Into::into).collect())
        }

        fn with_settlements(script: Vec<StepTurnSettlement>) -> Self {
            Self {
                application: Application::default(),
                script: Mutex::new(script.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn executor<'a>(
            &'a self,
            root: &Path,
            scope: ExecutionScope,
            config: ImplementationRunConfig,
        ) -> FixtureExecutor<'a> {
            let documents = self.application.documents_at(root);
            let operations = match scope {
                ExecutionScope::Plan { plan_id, step_id } => {
                    FixtureOperations::Plan(PlanLeaf::new(plan_id, step_id, documents), self)
                }
                ExecutionScope::ReviewGroup {
                    review_id,
                    group_id,
                } => FixtureOperations::Review(
                    ReviewLeafTarget {
                        documents,
                        review_id,
                        address: ReviewLeafAddress::Group(group_id),
                    },
                    self,
                ),
                ExecutionScope::ReviewFinding { review_id, item_id } => FixtureOperations::Review(
                    ReviewLeafTarget {
                        documents,
                        review_id,
                        address: ReviewLeafAddress::Finding(item_id),
                    },
                    self,
                ),
            };
            FixtureExecutor { operations, config }
        }

        fn requests(&self) -> MutexGuard<'_, Vec<StepTurnRequest>> {
            self.requests.lock().unwrap()
        }

        fn run_step(&self, request: StepTurnRequest) -> StepTurnSettlement {
            self.requests.lock().unwrap().push(request);
            self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                StepTurnResult::Failed(StepFailure::infrastructure("no scripted step result"))
                    .into()
            })
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
            commit: None,
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
                repl: vec![ModelEntry::from(ModelRef::openrouter("base/model"))],
                coder: Some(vec![ModelEntry::from(ModelRef::openrouter("base/model"))]),
                auto_level_up: true,
                ..ModelsConfig::default()
            },
            protocol: EditProtocolKind::Structured,
            marker: None,
            selection: None,
            instructions: None,
            queue: vec![step_id.to_string()],
        }
    }

    /// Coder ladder whose rungs each grant a single content attempt.
    fn two_rung_models() -> ModelsConfig {
        ModelsConfig {
            repl: vec![ModelEntry::from(ModelRef::openrouter("base/model"))],
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

    /// Coder ladder whose two rungs are one model at two reasoning depths, so a
    /// promotion deepens the depth instead of switching the model.
    fn single_model_ladder() -> ModelsConfig {
        let shared = ModelRef::openrouter("shared/model");
        ModelsConfig {
            repl: vec![ModelEntry::from(ModelRef::openrouter("base/model"))],
            coder: Some(vec![
                ModelEntry {
                    model: shared.clone(),
                    effort: Some(ReasoningEffort::Low),
                    attempts: Some(1),
                    level_up: None,
                },
                ModelEntry {
                    model: shared,
                    effort: Some(ReasoningEffort::High),
                    attempts: Some(1),
                    level_up: None,
                },
            ]),
            auto_level_up: true,
            ..ModelsConfig::default()
        }
    }
    /// Coder role pairing a participating rung with an opted-out model, so a run
    /// started on the opted-out model proves which entry owns the budget.
    fn excluded_model_models(attempts: Option<u32>) -> ModelsConfig {
        ModelsConfig {
            repl: vec![ModelEntry::from(ModelRef::openrouter("base/model"))],
            coder: Some(vec![
                ModelEntry {
                    model: ModelRef::openrouter("base/model"),
                    effort: None,
                    attempts: Some(2),
                    level_up: None,
                },
                ModelEntry {
                    model: ModelRef::openrouter("reserved/model"),
                    effort: None,
                    attempts,
                    level_up: Some(false),
                },
            ]),
            auto_level_up: true,
            ..ModelsConfig::default()
        }
    }
    /// Channels of a run, with the batch channel a step publishes its telemetry
    /// into exposed to the test.
    fn channels_with_events() -> (
        mpsc::Sender<StreamEvent>,
        mpsc::Receiver<StreamEvent>,
        watch::Receiver<bool>,
    ) {
        let (stream_tx, stream_rx) = mpsc::channel(8);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        (stream_tx, stream_rx, cancel_rx)
    }

    /// Channels of a run whose streamed telemetry no test observes.
    ///
    /// The receiver is dropped, so a promotion the step publishes reaches a
    /// closed channel and stays a no-op instead of parking the run.
    fn channels() -> (mpsc::Sender<StreamEvent>, watch::Receiver<bool>) {
        let (stream_tx, stream_rx, cancel_rx) = channels_with_events();
        drop(stream_rx);
        (stream_tx, cancel_rx)
    }

    #[tokio::test]
    async fn a_cancelled_plan_step_only_keeps_confirmed_progress() {
        for commit in [None, Some("abc1234")] {
            let dir = tempdir().unwrap();
            PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
            let host = ScriptedHost::with_settlements(vec![StepTurnSettlement {
                result: StepTurnResult::Cancelled,
                edit: commit.map(|hash| tauqe_core::edits::EditOutcome {
                    applied: true,
                    commit_hash: Some(hash.into()),
                    ..Default::default()
                }),
            }]);
            let scope = ExecutionScope::Plan {
                plan_id: "step-horizon".into(),
                step_id: None,
            };
            let executor = host.executor(dir.path(), scope, run_config("1"));
            let (stream_tx, cancel_rx) = channels();
            let outcome = executor.execute_leaf("1", stream_tx, cancel_rx).await;

            let LeafOutcome::Cancelled(Some(attempt)) = outcome else {
                panic!("expected a cancelled leaf with an attempt receipt");
            };
            assert_eq!(attempt.confirmed_commit(), commit);
            assert_eq!(host.requests().len(), 1);
            let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
                .unwrap()
                .unwrap();
            let leaf = stored.implementation.find_item("1").unwrap();
            let expected = if commit.is_some() {
                PlanItemStatus::Done
            } else {
                PlanItemStatus::Todo
            };
            assert_eq!(leaf.status, expected);
            assert_eq!(leaf.commit.as_deref(), commit);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_plan_step_is_marked_in_progress_and_done_through_the_plan_storage() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![StepTurnResult::Success {
            summary: "the seam is in place".to_string(),
            commit: Some("abc1234".to_string()),
        }]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
            step_id: None,
        };
        let executor = host.executor(dir.path(), scope, run_config("1"));
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Completed));
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        let completed = stored
            .implementation
            .find_item("1")
            .expect("the step is stored");
        assert_eq!(completed.status, PlanItemStatus::Done);
        assert_eq!(
            completed.commit.as_deref(),
            Some("abc1234"),
            "a completed step keeps the commit that produced it"
        );

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
            step_id: None,
        };
        let executor = host.executor(dir.path(), scope, run_config("1"));
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Blocked { step_id, reason }
            if step_id == "1" && reason == "the approved data structure cannot express the invariant"));
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        let step = stored.implementation.find_item("1").unwrap();
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
                commit: None,
            },
        ]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
            step_id: None,
        };
        let mut config = run_config("1");
        config.models = two_rung_models();
        let executor = host.executor(dir.path(), scope, config);
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Completed));
        let requests = host.requests();
        assert_eq!(requests.len(), 2);
        // The promoted rung travels as the whole `(model, effort)` pair, so the
        // selection names the exact ladder entry the run entered.
        assert_eq!(
            requests[1].selection,
            ModelSelection::Tier {
                model: ModelRef::openrouter("stronger/model"),
                effort: None,
            }
        );
        assert!(requests[1].prompt.contains("Correction Required"));
        assert!(requests[1].prompt.contains("Target Plan Step Execution"));
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().status,
            PlanItemStatus::Done
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_promotion_of_one_model_keeps_the_full_rung_and_its_session() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
            StepTurnResult::Success {
                summary: "the depth was enough".to_string(),
                commit: None,
            },
        ]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
            step_id: None,
        };
        let mut config = run_config("1");
        config.models = single_model_ladder();
        config.selection = Some(ModelSelection::Tier {
            model: ModelRef::openrouter("shared/model"),
            effort: Some(ReasoningEffort::Low),
        });
        let executor = host.executor(dir.path(), scope, config);
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor.execute_leaf("1", stream_tx, cancel_rx).await;

        assert!(matches!(outcome, LeafOutcome::Completed(_)));
        let requests = host.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].reasoning_effort, Some(ReasoningEffort::Low));
        // The promoted rung addresses the exact ladder entry the run entered:
        // naming only the model would collapse the step onto the base entry of
        // that model and lose the depth the promotion handed out.
        assert_eq!(
            requests[1].selection,
            ModelSelection::Tier {
                model: ModelRef::openrouter("shared/model"),
                effort: Some(ReasoningEffort::High),
            }
        );
        assert_eq!(requests[1].reasoning_effort, Some(ReasoningEffort::High));
        // Affinity follows the model alone, so deepening the depth of one model
        // keeps the provider session warm.
        assert_eq!(requests[1].session_id, requests[0].session_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_coder_promotion_is_published_on_the_batch_channel() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
            StepTurnResult::Success {
                summary: "the depth was enough".to_string(),
                commit: None,
            },
        ]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
            step_id: None,
        };
        let mut config = run_config("1");
        config.models = single_model_ladder();
        let executor = host.executor(dir.path(), scope, config);
        let (stream_tx, mut stream_rx, cancel_rx) = channels_with_events();

        let outcome = executor.execute_leaf("1", stream_tx, cancel_rx).await;

        assert!(matches!(outcome, LeafOutcome::Completed(_)));
        match stream_rx.try_recv() {
            Ok(StreamEvent::TierEscalated {
                tier,
                model,
                role,
                effort,
            }) => {
                assert_eq!(tier, 1);
                assert_eq!(model, ModelRef::openrouter("shared/model"));
                assert_eq!(role, ModelRole::Coder);
                assert_eq!(effort, Some(ReasoningEffort::High));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(
            stream_rx.try_recv().is_err(),
            "a promotion is published once per rung, not once per attempt"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exhausted_ladder_ends_the_step_without_a_retry() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
            StepTurnResult::Failed(StepFailure::domain("verification failed")),
        ]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
            step_id: None,
        };
        let mut config = run_config("1");
        config.models = two_rung_models();
        let executor = host.executor(dir.path(), scope, config);
        let (stream_tx, cancel_rx) = channels();

        let outcome = executor.execute_leaf("1", stream_tx, cancel_rx).await;

        // One rung, one attempt each: the second attempt runs on the promoted
        // rung and its own single attempt exhausts the ladder.
        assert_eq!(host.requests().len(), 2);
        assert!(
            matches!(outcome, LeafOutcome::Exhausted(reason, _) if reason.contains("verification failed"))
        );
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().status,
            PlanItemStatus::Todo
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_opted_out_model_runs_its_own_single_adaptive_attempt() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
        ]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
            step_id: None,
        };
        let mut config = run_config("1");
        config.models = excluded_model_models(None);
        config.selection = Some(ModelSelection::Specific(ModelRef::openrouter(
            "reserved/model",
        )));
        let executor = host.executor(dir.path(), scope, config);
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Failed { .. }));
        // The budget belongs to the selected entry and never to a ladder rung:
        // the opted-out model runs its single adaptive attempt instead of the
        // two attempts declared by the participating rung it saturates on.
        assert_eq!(host.requests().len(), 1);
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().status,
            PlanItemStatus::Todo
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_opted_out_model_honours_an_explicit_attempt_budget() {
        let dir = tempdir().unwrap();
        PlanStorage::save_plan(dir.path(), &horizon_plan()).unwrap();
        let host = ScriptedHost::new(vec![
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
            StepTurnResult::Failed(StepFailure::domain("the patch did not apply")),
        ]);
        let scope = ExecutionScope::Plan {
            plan_id: "step-horizon".to_string(),
            step_id: None,
        };
        let mut config = run_config("1");
        config.models = excluded_model_models(Some(3));
        config.selection = Some(ModelSelection::Specific(ModelRef::openrouter(
            "reserved/model",
        )));
        let executor = host.executor(dir.path(), scope, config);
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Failed { .. }));
        // An explicit budget always wins over the adaptive default, so the
        // opted-out model spends exactly the three attempts it declares.
        assert_eq!(host.requests().len(), 3);
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().status,
            PlanItemStatus::Todo
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_review_group_step_is_written_through_the_session_storage() {
        let dir = tempdir().unwrap();
        stored_review(dir.path());
        let host = ScriptedHost::new(vec![StepTurnResult::Success {
            summary: "the horizon advanced".to_string(),
            commit: Some("def5678".to_string()),
        }]);
        let scope = ExecutionScope::ReviewGroup {
            review_id: "review-horizon".to_string(),
            group_id: "arch-1".to_string(),
        };
        let executor = host.executor(dir.path(), scope, run_config("s-1"));
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Completed));
        let stored = ReviewStorage::load_review(dir.path(), "review-horizon")
            .unwrap()
            .unwrap();
        let group = stored
            .groups
            .iter()
            .find(|group| group.id == "arch-1")
            .expect("the group is stored");
        assert_eq!(group.implementation.items[0].status, PlanItemStatus::Done);
        assert_eq!(
            group.implementation.items[0].commit.as_deref(),
            Some("def5678"),
            "a completed sub-step keeps the commit that produced it"
        );
        // The finding statuses stay frozen while the horizon runs.
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_finding_step_marks_the_finding_in_progress() {
        let dir = tempdir().unwrap();
        stored_review(dir.path());
        let host = ScriptedHost::new(vec![StepTurnResult::Success {
            summary: "the finding is addressed".to_string(),
            commit: None,
        }]);
        let scope = ExecutionScope::ReviewFinding {
            review_id: "review-horizon".to_string(),
            item_id: 7,
        };
        let executor = host.executor(dir.path(), scope, run_config("7"));
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Completed));
        // The run owns the lifecycle of the finding, so the step it completed
        // leaves the finding InProgress; the verdict is settled by the pipeline.
        let stored = ReviewStorage::load_review(dir.path(), "review-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.items[0].status, IssueStatus::InProgress);
        let requests = host.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].prompt.contains("Target Plan Step Execution"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_completed_single_finding_step_records_the_commit_on_the_finding() {
        let dir = tempdir().unwrap();
        stored_review(dir.path());
        let host = ScriptedHost::new(vec![StepTurnResult::Success {
            summary: "the finding is addressed".to_string(),
            commit: Some("abc1234".to_string()),
        }]);
        let scope = ExecutionScope::ReviewFinding {
            review_id: "review-horizon".to_string(),
            item_id: 7,
        };
        let executor = host.executor(dir.path(), scope, run_config("7"));
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Completed));
        // The commit belongs to the completed record of the step, so the finding
        // points at the change set that resolved it even before the settlement
        // states the verdict.
        let stored = ReviewStorage::load_review(dir.path(), "review-horizon")
            .unwrap()
            .unwrap();
        let finding = stored.find_item(7).expect("the finding is stored");
        assert_eq!(finding.commit.as_deref(), Some("abc1234"));
        assert_eq!(finding.status, IssueStatus::InProgress);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reset_single_finding_step_returns_the_finding_to_todo() {
        let dir = tempdir().unwrap();
        stored_review(dir.path());
        let host = ScriptedHost::new(vec![StepTurnResult::Failed(StepFailure::domain(
            "the patch did not apply",
        ))]);
        let scope = ExecutionScope::ReviewFinding {
            review_id: "review-horizon".to_string(),
            item_id: 7,
        };
        let mut config = run_config("7");
        // One rung with a single attempt: one content failure exhausts the ladder.
        config.models.coder = Some(vec![ModelEntry {
            model: ModelRef::openrouter("base/model"),
            effort: None,
            attempts: Some(1),
            level_up: None,
        }]);
        let executor = host.executor(dir.path(), scope, config);
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Failed { .. }));
        let stored = ReviewStorage::load_review(dir.path(), "review-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_single_finding_step_returns_the_finding_to_todo() {
        let dir = tempdir().unwrap();
        stored_review(dir.path());
        let host = ScriptedHost::new(vec![StepTurnResult::Cancelled]);
        let scope = ExecutionScope::ReviewFinding {
            review_id: "review-horizon".to_string(),
            item_id: 7,
        };
        let executor = host.executor(dir.path(), scope, run_config("7"));
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Cancelled));
        let stored = ReviewStorage::load_review(dir.path(), "review-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(stored.items[0].status, IssueStatus::Todo);
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
            step_id: None,
        };
        let executor = host.executor(dir.path(), scope, run_config("1"));
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Failed { outcome, .. }
            if matches!(*outcome, LeafOutcome::Unreverted(_))));
        assert_eq!(host.requests().len(), 1);
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().status,
            PlanItemStatus::Todo
        );
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
            step_id: None,
        };
        let mut config = run_config("1");
        config.models = two_rung_models();
        let executor = host.executor(dir.path(), scope, config);
        let (stream_tx, cancel_rx) = channels();

        let stop = execute_queue(&executor, stream_tx, cancel_rx).await;
        assert!(matches!(stop, QueueStop::Failed { outcome, .. }
            if matches!(*outcome, LeafOutcome::Infrastructure(_))));
        // A transport failure is neutral to the tier budget, so nothing escalates.
        assert_eq!(host.requests().len(), 1);
        let stored = PlanStorage::load_plan(dir.path(), "step-horizon")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.implementation.find_item("1").unwrap().status,
            PlanItemStatus::Todo
        );
    }

    #[test]
    fn saturation_message_distinguishes_the_excluded_rung_from_its_sibling() {
        use tauqe_core::models::ReasoningEffort;

        let shared = ModelRef::openrouter("shared/model");
        let mut excluded = ModelEntry::new(shared.clone(), Some(ReasoningEffort::Max));
        excluded.level_up = Some(false);
        let models = ModelsConfig {
            repl: vec![ModelEntry::from(ModelRef::openrouter("base/model"))],
            coder: Some(vec![
                ModelEntry::new(shared.clone(), Some(ReasoningEffort::Low)),
                excluded,
                ModelEntry::from(ModelRef::openrouter("stronger/model")),
            ]),
            auto_level_up: true,
            ..ModelsConfig::default()
        };
        let plan = horizon_plan();
        let leaf = plan
            .implementation
            .items
            .first()
            .expect("the horizon declares one leaf");
        let reason = "the patch did not apply";

        // The opted-out rung reports the marker of its own entry ...
        let selection = ModelSelection::Tier {
            model: shared.clone(),
            effort: Some(ReasoningEffort::Max),
        };
        let message = super::saturation_message(&models, &selection, leaf, reason);
        assert!(
            message.contains("excluded from the automatic ladder"),
            "got: {message}"
        );
        assert!(message.contains("shared/model [max]"), "got: {message}");

        // ... while the participating sibling of the same model is reported as the
        // strongest rung, because no marker names it.
        let selection = ModelSelection::Tier {
            model: shared,
            effort: Some(ReasoningEffort::Low),
        };
        let message = super::saturation_message(&models, &selection, leaf, reason);
        assert!(
            message.contains("strongest rung of the automatic ladder"),
            "got: {message}"
        );
        assert!(message.contains("shared/model [low]"), "got: {message}");
    }
}
