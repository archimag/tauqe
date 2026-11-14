//! Plan step execution loop.
//!
//! [`run_plan_step_loop`] drives one leaf of a plan through the coder
//! ladder: it resolves the step from storage, prepares its declared context,
//! and runs the bounded turn loop per coder rung until the step reaches a
//! terminal state — `Done` with the commit that produced it, `Blocked` with
//! the reported reason, or a reset to `Todo` after the ladder is exhausted.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use tauqe_protocol::{
    ContextAccess, EditProtocolKind, LlmCapability, LlmCommand, ModelEntry, ModelRole,
    ModelSelection, Plan, PlanImplementation, PlanItem, PlanItemStatus, ReasoningEffort,
};

use crate::config::ModelsConfig;
use crate::control::context::LoopContext;
use crate::control::outcome::{
    FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError,
};
use crate::control::turn::{run_turn_loop, turn_capability_expr, TurnLoopConfig};
use crate::plan::prompt::{format_plan_step_execution_prompt, format_plan_step_retry_prompt};
use crate::plan::storage::PlanStorage;
use crate::prompt::TurnBrief;
use crate::providers::StreamEvent;
/// Persistence contract of one plan step lifecycle.
///
/// The bounded turn and the coder ladder are identical for every horizon of
/// steps; only the document that records the lifecycle differs. An adapter owns
/// loading and persistence, so one loop serves a local plan and a review group
/// subplan alike.
pub trait StepPersistence: Send + Sync {
    /// Loads the document that carries the executed steps.
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError>;

    /// Makes the lifecycle recorded in `plan` durable.
    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError>;
}

/// Persistence of a step lifecycle recorded by a local plan document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDocumentPersistence {
    /// Identifier of the plan the executed steps belong to.
    pub plan_id: String,
}

impl PlanDocumentPersistence {
    /// Addresses the plan document identified by `plan_id`.
    pub fn new(plan_id: impl Into<String>) -> Self {
        Self {
            plan_id: plan_id.into(),
        }
    }
}

impl StepPersistence for PlanDocumentPersistence {
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError> {
        load_plan(repo_root, &self.plan_id)
    }

    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError> {
        PlanStorage::save_plan(repo_root, plan).map_err(|err| {
            SystemError::Other(format!(
                "Failed to persist the step lifecycle of plan '{}': {}",
                self.plan_id, err
            ))
        })
    }
}
/// Configuration of one plan step execution.
#[derive(Debug, Clone)]
pub struct PlanStepLoopConfig {
    /// Configuration of the underlying bounded turn.
    pub turn: TurnLoopConfig,
    /// Coder ladder and automatic escalation policy.
    pub models: ModelsConfig,
    /// Protocol that shapes the step prompt and its control tags.
    pub protocol: EditProtocolKind,
    /// Turn marker bound to the tagged control channel.
    pub marker: Option<String>,
    /// Coder selection the step starts from.
    pub selection: Option<ModelSelection>,
}

/// Artifact of a successful plan step execution.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanStepArtifact {
    /// Identifier of the step that succeeded.
    pub step_id: String,
    /// Commit hash produced by the step, when its turn made one.
    pub commit: Option<String>,
    /// Repository-relative paths the step modified.
    pub modified_paths: Vec<String>,
}

/// Runs the plan step lifecycle for one leaf of a local plan.
///
/// The step is resolved from the plan document, its declared context is
/// prepared, and the coder ladder drives one attempt per rung until the step
/// succeeds, blocks, or the ladder is exhausted. `Done` and `Blocked` write the
/// durable state through [`PlanDocumentPersistence`]; every other outcome leaves
/// the step at `Todo` so no completed turn is claimed by accident.
pub async fn run_plan_step_loop(
    ctx: &mut LoopContext<'_>,
    plan_id: &str,
    step_id: &str,
    config: &PlanStepLoopConfig,
) -> Result<LoopResult<PlanStepArtifact>, SystemError> {
    let persistence = PlanDocumentPersistence::new(plan_id);
    run_plan_step_loop_with(ctx, &persistence, step_id, config).await
}

/// Runs the plan step lifecycle against any document that records the steps.
///
/// The bounded turn, the coder ladder, and the lifecycle rules are identical for
/// every horizon of steps, so the persistence adapter owns loading and writing
/// while this loop stays the one implementation of the lifecycle.
pub async fn run_plan_step_loop_with<P: StepPersistence>(
    ctx: &mut LoopContext<'_>,
    persistence: &P,
    step_id: &str,
    config: &PlanStepLoopConfig,
) -> Result<LoopResult<PlanStepArtifact>, SystemError> {
    ctx.check_cancelled()?;
    // The step owns its introductory brief: the loop that drives the turn declares
    // the contour and the text it opens with.
    ctx.set_turn_brief(Some(TurnBrief::step()));
    // The harness state directory lives under `.tauqe/`, and a repository that
    // has never opened a transaction does not exclude it yet. The exclusion is
    // installed here, so the first snapshot of the first turn cannot observe the
    // harness state directory as a change set member and `stage_files` cannot
    // hand Git a gitignored path. The call is idempotent and covers a fresh
    // repository as well.
    crate::git::checkpoint::ensure_tauqe_ignored(ctx.repo_root).map_err(|err| {
        SystemError::Git(format!(
            "Failed to install the .tauqe exclusion before executing step '{}': {err}",
            step_id
        ))
    })?;

    let mut plan = persistence.load(ctx.repo_root)?;

    {
        let leaf = resolve_leaf(&plan, step_id)?;

        // Refuse a step that cannot run; a Done step is idempotent.
        match leaf.status {
            PlanItemStatus::Done => {
                return Ok(LoopResult::Success(LoopSuccess::new(
                    PlanStepArtifact {
                        step_id: leaf.id.clone(),
                        commit: leaf.commit.clone(),
                        modified_paths: Vec::new(),
                    },
                    ResponseEnvelope::default(),
                )));
            }
            PlanItemStatus::Discussion => {
                return Err(SystemError::Other(format!(
                    "Step '{}' ({}) is in DISCUSSION status and cannot be executed until reviewed",
                    leaf.id, leaf.title
                )));
            }
            PlanItemStatus::Blocked => {
                return Err(SystemError::Other(format!(
                    "Step '{}' ({}) is BLOCKED; resolve the reported blocker and replan before executing",
                    leaf.id, leaf.title
                )));
            }
            PlanItemStatus::Cancelled => {
                return Err(SystemError::Other(format!(
                    "Step '{}' ({}) is CANCELLED and cannot be executed",
                    leaf.id, leaf.title
                )));
            }
            PlanItemStatus::Todo | PlanItemStatus::InProgress => {}
        }

        prepare_context(ctx, leaf)?;
    }

    let mut prompt = format_plan_step_execution_prompt(
        &plan,
        step_id,
        config.protocol,
        config.marker.as_deref(),
    )
    .map_err(SystemError::Other)?;

    transition(
        ctx.repo_root,
        persistence,
        &mut plan,
        step_id,
        PlanItemStatus::InProgress,
        None,
        None,
    )?;

    let start_selection = config
        .selection
        .clone()
        .unwrap_or(ModelSelection::Role(ModelRole::Coder));
    let mut ladder_idx = config.models.coder_tier_index(Some(&start_selection));
    let mut reasoning_effort: Option<ReasoningEffort> = config
        .models
        .resolve_selection_effort(ModelRole::Coder, &start_selection);
    let mut tier_attempts = 0u32;
    let mut max_tier_attempts = config
        .models
        .resolve_selection_attempts(ModelRole::Coder, &start_selection);
    let mut current_model_name = config.models.coder_tier(ladder_idx).model.to_string();
    loop {
        ctx.check_cancelled()?;

        // The turn loop reuses the same prompt builder across attempts, so
        // the attempt prompt and its reasoning depth are written here.
        ctx.prompt_builder.prompt = Some(prompt.clone());
        ctx.prompt_builder.reasoning_effort = reasoning_effort;
        let turn_result = {
            let turn_caps = turn_capability_expr();
            let mut turn_ctx = ctx.reborrow_with_capabilities(turn_caps);
            run_turn_loop(&mut turn_ctx, None, &config.turn).await?
        };

        let escalation_trigger: Option<String> = match turn_result {
            LoopResult::Success(success) => {
                let artifact = success.value;

                // A change set that reached disk but could not be committed
                // left the working tree in a state nobody verified: reset and
                // surface the failure instead of retrying on top of it.
                if let Some(err) = artifact.commit_error.as_ref() {
                    transition(
                        ctx.repo_root,
                        persistence,
                        &mut plan,
                        step_id,
                        PlanItemStatus::Todo,
                        None,
                        None,
                    )?;
                    return Err(SystemError::Git(format!(
                        "Failed to commit the change set of step '{}': {}",
                        step_id, err
                    )));
                }

                if let Some(commit) = artifact.commit_hash.clone() {
                    // A committed change set means the step is Done.
                    transition(
                        ctx.repo_root,
                        persistence,
                        &mut plan,
                        step_id,
                        PlanItemStatus::Done,
                        None,
                        Some(commit.as_str()),
                    )?;
                    return Ok(LoopResult::Success(LoopSuccess::new(
                        PlanStepArtifact {
                            step_id: step_id.to_string(),
                            commit: Some(commit),
                            modified_paths: artifact.modified_paths,
                        },
                        success.response,
                    )));
                }

                // A divergence declared through `<blocked>` short-circuits the
                // ladder: a stronger coder would meet the same invariant.
                if let Some((reason, details)) = extract_blocked(&success.response) {
                    let combined = combine_blocked(reason.as_str(), details.as_str());
                    transition(
                        ctx.repo_root,
                        persistence,
                        &mut plan,
                        step_id,
                        PlanItemStatus::Blocked,
                        Some(combined.as_str()),
                        None,
                    )?;
                    let mut env = success.response;
                    let _ = env.take(LlmCapability::Blocked);
                    let _ = env.take_any(&[
                        LlmCapability::CommitMessage,
                        LlmCapability::TurnError,
                        LlmCapability::TurnSkip,
                    ]);
                    return Ok(LoopResult::Diverged(env));
                }

                // A terminal outcome without edits also means Done.
                match extract_terminal(&success.response) {
                    Some(LlmCommand::CommitMessage(_)) | Some(LlmCommand::TurnSkip(_)) => {
                        transition(
                            ctx.repo_root,
                            persistence,
                            &mut plan,
                            step_id,
                            PlanItemStatus::Done,
                            None,
                            None,
                        )?;
                        return Ok(LoopResult::Success(LoopSuccess::new(
                            PlanStepArtifact {
                                step_id: step_id.to_string(),
                                commit: None,
                                modified_paths: artifact.modified_paths,
                            },
                            success.response,
                        )));
                    }
                    Some(LlmCommand::TurnError(command)) => {
                        Some(terminal_error_message(&command.error_message))
                    }
                    _ => Some(
                        "The step turn produced neither a verified change set nor a declared outcome"
                            .to_string(),
                    ),
                }
            }
            LoopResult::Diverged(mut envelope) => {
                if let Some((reason, details)) = extract_blocked(&envelope) {
                    let combined = combine_blocked(reason.as_str(), details.as_str());
                    transition(
                        ctx.repo_root,
                        persistence,
                        &mut plan,
                        step_id,
                        PlanItemStatus::Blocked,
                        Some(combined.as_str()),
                        None,
                    )?;
                    let _ = envelope.take(LlmCapability::Blocked);
                    let _ = envelope.take_any(&[
                        LlmCapability::CommitMessage,
                        LlmCapability::TurnError,
                        LlmCapability::TurnSkip,
                    ]);
                }
                Some("The step turn diverged without a completed change set".to_string())
            }
            LoopResult::Exhausted(report) => Some(report.reason),
        };

        let trigger = escalation_trigger
            .unwrap_or_else(|| "The step turn did not produce a verifiable result".to_string());

        tier_attempts += 1;
        if tier_attempts >= max_tier_attempts {
            match next_rung(config, ladder_idx) {
                None => {
                    transition(
                        ctx.repo_root,
                        persistence,
                        &mut plan,
                        step_id,
                        PlanItemStatus::Todo,
                        None,
                        None,
                    )?;
                    return Ok(LoopResult::Exhausted(FailureReport::new(
                        format!("{trigger}: escalation is exhausted on {current_model_name}"),
                        tier_attempts as usize,
                        Vec::new(),
                    )));
                }
                Some((next_idx, tier)) => {
                    ctx.events.emit(StreamEvent::TierEscalated {
                        tier: next_idx,
                        model: tier.model.clone(),
                        role: ModelRole::Coder,
                        effort: tier.effort,
                    });
                    ladder_idx = next_idx;
                    current_model_name = tier.model.to_string();
                    max_tier_attempts = tier.attempts();
                    reasoning_effort = tier.effort;
                    tier_attempts = 0;
                    prompt = format_plan_step_retry_prompt(
                        "",
                        Some(trigger.as_str()),
                        config.protocol,
                        config.marker.as_deref(),
                    );
                }
            }
        } else {
            prompt = format_plan_step_retry_prompt(
                "",
                Some(trigger.as_str()),
                config.protocol,
                config.marker.as_deref(),
            );
        }
    }
}

fn load_plan(repo_root: &Path, plan_id: &str) -> Result<Plan, SystemError> {
    PlanStorage::load_plan(repo_root, plan_id)
        .map_err(|e| SystemError::Other(format!("Failed to load plan '{}': {}", plan_id, e)))?
        .ok_or_else(|| SystemError::Other(format!("Plan '{}' not found", plan_id)))
}

fn resolve_leaf<'a>(plan: &'a Plan, step_id: &str) -> Result<&'a PlanItem, SystemError> {
    let item = plan.implementation.find_item(step_id).ok_or_else(|| {
        SystemError::Other(format!(
            "Step '{}' is not part of plan '{}'",
            step_id, plan.id
        ))
    })?;
    if !item.is_leaf() {
        return Err(SystemError::Other(format!(
            "Step '{}' ({}) is a container of the implementation horizon and never an execution target",
            item.id, item.title
        )));
    }
    Ok(item)
}

fn prepare_context(ctx: &mut LoopContext<'_>, leaf: &PlanItem) -> Result<(), SystemError> {
    for path in &leaf.files {
        let full_path = ctx.repo_root.join(path);
        if full_path.is_file() {
            ctx.context_manager
                .add_file(path, ContextAccess::Editable)
                .map_err(|e| {
                    SystemError::Other(format!(
                        "Failed to add target file '{}' of step '{}' to context: {}",
                        path, leaf.id, e
                    ))
                })?;
        }
    }
    for path in &leaf.readonly_files {
        ctx.context_manager
            .add_file(path, ContextAccess::ReadOnly)
            .map_err(|e| {
                SystemError::Other(format!(
                    "Failed to add contract file '{}' of step '{}' to context: {}",
                    path, leaf.id, e
                ))
            })?;
    }
    Ok(())
}

/// Applies one lifecycle transition to the executed step and makes it durable.
///
/// The transition belongs to the document that records it, so the same rules run
/// for a local plan and for a review group horizon: entering `Blocked` stores the
/// reported diagnosis, leaving it drops the stale report, and a commit travels
/// only with the transition that finished the step.
fn transition<P: StepPersistence>(
    repo_root: &Path,
    persistence: &P,
    plan: &mut Plan,
    step_id: &str,
    status: PlanItemStatus,
    blocked_reason: Option<&str>,
    commit: Option<&str>,
) -> Result<(), SystemError> {
    if !apply_step_transition(
        &mut plan.implementation,
        step_id,
        status,
        blocked_reason,
        commit,
    ) {
        return Err(SystemError::Other(format!(
            "Step '{}' is not part of the executed plan '{}'",
            step_id, plan.id
        )));
    }
    plan.updated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    persistence.persist(repo_root, plan)
}

/// Applies one lifecycle transition to a step of an implementation horizon.
///
/// The step keeps its own verdict and the containers above it are recomputed, so
/// a horizon never reports a container that disagrees with the leaves it
/// aggregates. Returns whether the target step exists.
fn apply_step_transition(
    implementation: &mut PlanImplementation,
    step_id: &str,
    status: PlanItemStatus,
    blocked_reason: Option<&str>,
    commit: Option<&str>,
) -> bool {
    fn visit(
        items: &mut [PlanItem],
        target_id: &str,
        status: PlanItemStatus,
        blocked_reason: Option<&str>,
        commit: Option<&str>,
    ) -> bool {
        for item in items {
            if item.id.eq_ignore_ascii_case(target_id) {
                item.status = status;
                if let Some(commit) = commit.map(str::trim).filter(|hash| !hash.is_empty()) {
                    item.commit = Some(commit.to_string());
                }
                // The diagnostic report belongs to the Blocked state alone:
                // entering it stores the supplied reason, leaving it drops the
                // stale report so the stored document never keeps describing a
                // blocker that has been resolved already.
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
            if visit(
                &mut item.children,
                target_id,
                status,
                blocked_reason,
                commit,
            ) {
                return true;
            }
        }
        false
    }

    let found = visit(
        &mut implementation.items,
        step_id,
        status,
        blocked_reason,
        commit,
    );
    if found {
        implementation.refresh_parent_statuses();
        implementation.normalize_lifecycle_provenance();
    }
    found
}

fn extract_terminal(envelope: &ResponseEnvelope) -> Option<LlmCommand> {
    let mut env = envelope.clone();
    env.take_any(&[
        LlmCapability::CommitMessage,
        LlmCapability::TurnError,
        LlmCapability::TurnSkip,
    ])
    .into_iter()
    .next()
}

/// Renders the cause of a step turn that declared a terminal error.
fn terminal_error_message(message: &str) -> String {
    let message = message.trim();
    if message.is_empty() {
        "The step turn reported an error without stating a cause".to_string()
    } else {
        message.to_string()
    }
}

fn extract_blocked(envelope: &ResponseEnvelope) -> Option<(String, String)> {
    let mut env = envelope.clone();
    env.take(LlmCapability::Blocked)
        .into_iter()
        .find_map(|cmd| match cmd {
            LlmCommand::Blocked(command) => Some((command.reason, command.details)),
            _ => None,
        })
}

fn combine_blocked(reason: &str, details: &str) -> String {
    let details = details.trim();
    if details.is_empty() {
        reason.trim().to_string()
    } else {
        format!("{}\n\n{}", reason.trim(), details)
    }
}

fn next_rung(config: &PlanStepLoopConfig, ladder_idx: usize) -> Option<(usize, ModelEntry)> {
    if config.models.auto_level_up {
        config.models.next_coder_tier(ladder_idx)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use tauqe_protocol::{CapabilityExpr, ConceptItem, ModelRef, Plan, PlanItem, PlanItemStatus};

    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink, LoopContext};
    use crate::control::prompt::PromptBuilder;
    use crate::control::turn::TurnLoopConfig;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: JsonPromptProtocol,
        cache: crate::control::cache::TurnInputCache,
        cancel: CancellationToken,
        events: EventSink,
        policy: crate::workflow::git_policy::GitTransactionPolicy,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)], responses: Vec<MockResponse>) -> Self {
            let dir = tempfile::tempdir().expect("scratch directory");
            let root = dir.path().to_path_buf();
            crate::git::init_repository(Some(&root), true).expect("repository");
            for (path, content) in files {
                let full = root.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("dir");
                }
                std::fs::write(full, content).expect("file");
            }
            let owned: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
            if !owned.is_empty() {
                crate::git::cmd::stage_files(&root, &owned).expect("stage");
                crate::git::cmd::commit_with_developer_author(&root, "Initial commit", false)
                    .expect("commit");
            }
            Self {
                dir,
                context: ContextManager::new(root),
                journal: TurnResearchJournal::default(),
                provider: MockLlmProvider::scripted(responses),
                model: ModelRef::openrouter("mock/model"),
                protocol: JsonPromptProtocol::edits(),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::git_policy::GitTransactionPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilityExpr::set([CapabilityExpr::leaf(LlmCapability::TextMessage)]);
            let prompt_builder =
                PromptBuilder::new(&self.provider, &self.model, &self.protocol, &mut self.cache);
            LoopContext::new(
                &self.cancel,
                &self.events,
                self.dir.path(),
                &mut self.context,
                &mut self.journal,
                &mut self.policy,
                caps,
                prompt_builder,
            )
        }
    }

    fn test_config() -> PlanStepLoopConfig {
        PlanStepLoopConfig {
            turn: TurnLoopConfig::default(),
            models: ModelsConfig::default(),
            protocol: EditProtocolKind::Structured,
            marker: None,
            selection: None,
        }
    }

    fn stored_todo_plan(root: &Path, id: &str) -> Plan {
        let plan = Plan::from_parts(
            id.to_string(),
            "Test Plan".to_string(),
            Some("A test plan".to_string()),
            0,
            0,
            vec![ConceptItem {
                id: "c-1".to_string(),
                title: "Concept".to_string(),
                details: None,
                children: Vec::new(),
            }],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["hello.rs".to_string()],
                ..PlanItem::default()
            }],
        );
        PlanStorage::save_plan(root, &plan).expect("save");
        plan
    }

    #[tokio::test]
    async fn step_loop_marks_the_leaf_done_with_the_commit() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![MockResponse::text(
                serde_json::json!({
                    "message": "Update hello",
                    "action": {
                        "kind": "edit",
                        "changes": [{
                            "op": "replace",
                            "path": "hello.rs",
                            "to": "",
                            "start": "",
                            "end": "",
                            "content": "",
                            "old_text": "fn hello() {}\n",
                            "new_text": "fn hello() -> i32 { 1 }\n"
                        }],
                        "commit_message": "feat(hello): return a value",
                        "message": "",
                        "reason": ""
                    }
                })
                .to_string(),
            )],
        );
        let root = fixture.dir.path().to_path_buf();
        stored_todo_plan(&root, "step-plan");

        let mut ctx = fixture.context();
        let outcome = run_plan_step_loop(&mut ctx, "step-plan", "1", &test_config())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.step_id, "1");
        assert!(success.value.commit.is_some());
        assert_eq!(success.value.modified_paths, vec!["hello.rs".to_string()]);

        // The loop owns the brief of its contour, so the assembled prompt opens
        // with the step introduction and states the step rules instead of the
        // dialogue authorization rule.
        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 1);
        let system = &requests[0].messages[0].content;
        assert!(system.contains("Tauqe Coder"));
        assert!(!system.contains("proactive engineering assistant"));
        assert!(system.contains("Step Execution Scope"));
        assert!(system.contains("Architectural Decision Immutability"));
        assert!(!system.contains("Action Authorization"));

        let stored = PlanStorage::load_plan(&root, "step-plan")
            .expect("load")
            .expect("exists");
        let leaf = stored.implementation.find_item("1").expect("leaf");
        assert_eq!(leaf.status, PlanItemStatus::Done);
        assert!(leaf.commit.is_some());
    }

    #[tokio::test]
    async fn step_loop_refuses_a_container_step() {
        let mut fixture = Fixture::new(&[], vec![]);
        let root = fixture.dir.path().to_path_buf();
        let plan = Plan::from_parts(
            "step-plan".to_string(),
            "Test Plan".to_string(),
            None,
            0,
            0,
            Vec::new(),
            vec![PlanItem {
                id: "1".to_string(),
                title: "Parent".to_string(),
                status: PlanItemStatus::Todo,
                children: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Leaf".to_string(),
                    files: vec!["hello.rs".to_string()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        );
        PlanStorage::save_plan(&root, &plan).expect("save");

        let mut ctx = fixture.context();
        let err = run_plan_step_loop(&mut ctx, "step-plan", "1", &test_config())
            .await
            .expect_err("container must be refused");
        assert!(err.to_string().contains("container"));
    }

    #[tokio::test]
    async fn step_loop_returns_early_for_a_done_step() {
        let mut fixture = Fixture::new(&[("hello.rs", "fn hello() {}\n")], vec![]);
        let root = fixture.dir.path().to_path_buf();
        let plan = Plan::from_parts(
            "step-plan".to_string(),
            "Test Plan".to_string(),
            None,
            0,
            0,
            Vec::new(),
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                status: PlanItemStatus::Done,
                commit: Some("abc1234".to_string()),
                files: vec!["hello.rs".to_string()],
                ..Default::default()
            }],
        );
        PlanStorage::save_plan(&root, &plan).expect("save");

        let mut ctx = fixture.context();
        let outcome = run_plan_step_loop(&mut ctx, "step-plan", "1", &test_config())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.commit.as_deref(), Some("abc1234"));
        assert!(success.value.modified_paths.is_empty());
    }
    #[tokio::test]
    async fn step_loop_tolerates_nonexistent_target_file_for_creation() {
        let mut fixture = Fixture::new(
            &[("init.rs", "// initial\n")],
            vec![MockResponse::text(
                serde_json::json!({
                    "message": "Create new file",
                    "action": {
                        "kind": "edit",
                        "changes": [{
                            "op": "create",
                            "path": "new_file.rs",
                            "to": "",
                            "start": "",
                            "end": "",
                            "content": "fn new_file() {}\n"
                        }],
                        "commit_message": "feat(new): create new file",
                        "message": "",
                        "reason": ""
                    }
                })
                .to_string(),
            )],
        );
        let root = fixture.dir.path().to_path_buf();
        let plan = Plan::from_parts(
            "create-plan".to_string(),
            "Create Plan".to_string(),
            None,
            0,
            0,
            Vec::new(),
            vec![PlanItem {
                id: "1".to_string(),
                title: "Create Step".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["new_file.rs".to_string()],
                ..Default::default()
            }],
        );
        PlanStorage::save_plan(&root, &plan).expect("save");

        let mut ctx = fixture.context();
        let outcome = run_plan_step_loop(&mut ctx, "create-plan", "1", &test_config())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.step_id, "1");
        assert!(success.value.commit.is_some());
    }
}
