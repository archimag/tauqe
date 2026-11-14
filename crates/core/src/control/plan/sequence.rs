//! Plan sequence execution loop.
//!
//! [`run_plan_sequence_loop`] drives an ordered sequence of plan steps
//! (such as a review group subplan, a range of steps, or a linear plan)
//! through [`run_plan_step_loop_with`], observing soft-stop and cancellation
//! signals between steps. The persistence adapter decides which document records
//! the lifecycle, so a local plan and a review group horizon share one loop.

use std::path::Path;

use tokio::sync::watch;

use tauqe_protocol::{IssueStatus, Plan, PlanItemStatus};

use crate::control::context::LoopContext;
use crate::control::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use crate::control::plan::step::{
    run_plan_step_loop_with, PlanDocumentPersistence, PlanStepArtifact, PlanStepLoopConfig,
    StepPersistence,
};
use crate::review::ReviewStorage;
use crate::workflow::implementation::ExecutionScope;
/// Configuration for running a sequence of plan steps.
#[derive(Debug, Clone)]
pub struct PlanSequenceLoopConfig {
    /// Configuration applied to each individual step.
    pub step: PlanStepLoopConfig,
}

impl From<PlanStepLoopConfig> for PlanSequenceLoopConfig {
    fn from(step: PlanStepLoopConfig) -> Self {
        Self { step }
    }
}

/// Terminal stop status of a plan step sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSequenceStop {
    /// Every scheduled step was executed to completion.
    Completed,
    /// A soft stop was observed between steps, halting the sequence gracefully.
    Stopped,
}

impl PlanSequenceStop {
    /// Returns `true` if all scheduled steps completed.
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
    }

    /// Returns `true` if the sequence was halted by a soft stop.
    pub fn is_stopped(&self) -> bool {
        matches!(self, Self::Stopped)
    }
}

/// Artifact of a successfully completed or gracefully stopped sequence of plan steps.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSequenceArtifact {
    /// Artifacts of the individual steps that ran and succeeded.
    pub executed_steps: Vec<PlanStepArtifact>,
    /// Reason the sequence finished.
    pub stop: PlanSequenceStop,
}

impl PlanSequenceArtifact {
    /// Returns `true` if every step in the sequence completed successfully.
    pub fn is_success(&self) -> bool {
        self.stop.is_completed()
    }
}

/// Persistence of the remediation horizon owned by one review group.
///
/// A review group fixes one root cause through one coherent change set, so its
/// findings share a single sequence of steps. That horizon is a plain
/// implementation of steps, which is why it runs through the same sequence loop
/// as a local plan: only the document that records the lifecycle differs, and it
/// is the review session that owns the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewGroupPersistence {
    /// Identifier of the review session that owns the group.
    pub review_id: String,
    /// Identifier of the group whose horizon is executed.
    pub group_id: String,
}

impl ReviewGroupPersistence {
    /// Addresses the horizon of one review group.
    pub fn new(review_id: impl Into<String>, group_id: impl Into<String>) -> Self {
        Self {
            review_id: review_id.into(),
            group_id: group_id.into(),
        }
    }
}

impl StepPersistence for ReviewGroupPersistence {
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError> {
        let session = ReviewStorage::load_review(repo_root, &self.review_id)
            .map_err(|err| {
                SystemError::Other(format!(
                    "Failed to load review session '{}': {}",
                    self.review_id, err
                ))
            })?
            .ok_or_else(|| {
                SystemError::Other(format!("Review session '{}' not found", self.review_id))
            })?;
        let group = session
            .groups
            .iter()
            .find(|group| group.id == self.group_id)
            .ok_or_else(|| {
                SystemError::Other(format!(
                    "Review session '{}' declares no group '{}'",
                    self.review_id, self.group_id
                ))
            })?;

        // The step loop reads a document, and the horizon of a group is exactly
        // that: the steps it declares. The header the loop never reads carries
        // the identity of the group the steps belong to.
        Ok(Plan::from_parts(
            session.id.clone(),
            group.title.clone(),
            group.description.clone(),
            0,
            0,
            Vec::new(),
            group.implementation.items.clone(),
        ))
    }

    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError> {
        let updated = ReviewStorage::update_group_subplan(
            repo_root,
            &self.review_id,
            &self.group_id,
            plan.implementation.items.clone(),
        )
        .map_err(|err| {
            SystemError::Other(format!(
                "Failed to persist the horizon of review group '{}' in session '{}': {}",
                self.group_id, self.review_id, err
            ))
        })?;

        match updated {
            Some(_) => Ok(()),
            None => Err(SystemError::Other(format!(
                "Review session '{}' declares no group '{}'",
                self.review_id, self.group_id
            ))),
        }
    }
}

/// Persistence of the degenerate one-step horizon a single review finding owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFindingPersistence {
    /// Identifier of the review session that owns the finding.
    pub review_id: String,
    /// Audit ordinal of the executed finding.
    pub item_id: u32,
}

impl ReviewFindingPersistence {
    /// Addresses the degenerate horizon of one review finding.
    pub fn new(review_id: impl Into<String>, item_id: u32) -> Self {
        Self {
            review_id: review_id.into(),
            item_id,
        }
    }
}

impl StepPersistence for ReviewFindingPersistence {
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError> {
        let scope = ExecutionScope::ReviewFinding {
            review_id: self.review_id.clone(),
            item_id: self.item_id,
        };
        scope
            .resolve(repo_root)
            .map(|frame| frame.plan)
            .map_err(|failure| SystemError::Other(failure.into_reason()))
    }

    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError> {
        let step_id = self.item_id.to_string();
        let Some(step) = plan.implementation.find_item(&step_id) else {
            return Err(SystemError::Other(format!(
                "The degenerate horizon of review finding #{} does not carry the step '{}'",
                self.item_id, step_id
            )));
        };
        let commit = step.commit.clone();
        let persisted = match step.status {
            PlanItemStatus::InProgress => ReviewStorage::update_session_item_status(
                repo_root,
                &self.review_id,
                self.item_id,
                IssueStatus::InProgress,
            ),
            PlanItemStatus::Todo => ReviewStorage::update_session_item_status(
                repo_root,
                &self.review_id,
                self.item_id,
                IssueStatus::Todo,
            ),
            // A completed step records the commit of the change set that carried
            // it. The verdict of the finding is never written here: it is stated
            // exactly once, by the settlement of the run that addressed it.
            PlanItemStatus::Done => ReviewStorage::update_session_item(
                repo_root,
                &self.review_id,
                self.item_id,
                |issue| {
                    if let Some(commit) = commit
                        .as_deref()
                        .map(str::trim)
                        .filter(|hash| !hash.is_empty())
                    {
                        issue.commit = Some(commit.to_string());
                    }
                },
            ),
            PlanItemStatus::Blocked | PlanItemStatus::Discussion | PlanItemStatus::Cancelled => {
                return Ok(())
            }
        };
        persisted.map_err(|err| {
            SystemError::Other(format!(
                "Failed to persist the lifecycle of review finding #{}: {}",
                self.item_id, err
            ))
        })?;
        Ok(())
    }
}

/// Persistence of the horizon a review batch member executes.
///
/// A member addresses either the unified horizon of an architectural group or
/// the degenerate horizon of a single finding, so the adapter selects the
/// storage that owns the addressed entity and the step loop stays unaware of
/// which one it drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewMemberPersistence {
    /// The unified horizon of an architectural review group.
    Group(ReviewGroupPersistence),
    /// The degenerate one-step horizon of a single review finding.
    Finding(ReviewFindingPersistence),
}

impl ReviewMemberPersistence {
    /// Selects the persistence of the review entity a scope addresses.
    pub fn of(scope: &ExecutionScope) -> Option<Self> {
        match scope {
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => Some(Self::Group(ReviewGroupPersistence::new(
                review_id.as_str(),
                group_id.as_str(),
            ))),
            ExecutionScope::ReviewFinding { review_id, item_id } => Some(Self::Finding(
                ReviewFindingPersistence::new(review_id.as_str(), *item_id),
            )),
            ExecutionScope::Plan { .. } => None,
        }
    }
}

impl StepPersistence for ReviewMemberPersistence {
    fn load(&self, repo_root: &Path) -> Result<Plan, SystemError> {
        match self {
            Self::Group(group) => group.load(repo_root),
            Self::Finding(finding) => finding.load(repo_root),
        }
    }

    fn persist(&self, repo_root: &Path, plan: &Plan) -> Result<(), SystemError> {
        match self {
            Self::Group(group) => group.persist(repo_root, plan),
            Self::Finding(finding) => finding.persist(repo_root, plan),
        }
    }
}

/// Drives an ordered sequence of plan steps sequentially.
///
/// Each step is executed via
/// [`run_plan_step_loop_with`](super::step::run_plan_step_loop_with). Between
/// steps, cooperative cancellation (`ctx.check_cancelled()`) and optional
/// soft-stop signals are observed:
/// - Hard cancellation takes precedence and immediately aborts the loop with [`SystemError::Cancelled`].
/// - A soft stop observed between steps terminates the sequence cleanly and returns
///   [`PlanSequenceStop::Stopped`], leaving the completed steps intact.
/// - If a step diverges (e.g. `<blocked>`) or exhausts its coder ladder, execution halts
///   and propagates the [`LoopResult::Diverged`] or [`LoopResult::Exhausted`] outcome.
pub async fn run_plan_sequence_loop<S: AsRef<str>>(
    ctx: &mut LoopContext<'_>,
    plan_id: &str,
    step_ids: &[S],
    config: &PlanSequenceLoopConfig,
    soft_stop_rx: Option<&watch::Receiver<bool>>,
) -> Result<LoopResult<PlanSequenceArtifact>, SystemError> {
    let persistence = PlanDocumentPersistence::new(plan_id);
    run_plan_sequence_loop_with(ctx, &persistence, step_ids, config, soft_stop_rx).await
}

/// Drives an ordered sequence of steps recorded by any document.
///
/// The sequence semantics belong to this loop alone: a local plan and a review
/// group horizon differ only in the adapter that loads and persists the steps, so
/// both observe the same soft-stop boundary and the same cancellation precedence.
pub async fn run_plan_sequence_loop_with<S: AsRef<str>, P: StepPersistence>(
    ctx: &mut LoopContext<'_>,
    persistence: &P,
    step_ids: &[S],
    config: &PlanSequenceLoopConfig,
    soft_stop_rx: Option<&watch::Receiver<bool>>,
) -> Result<LoopResult<PlanSequenceArtifact>, SystemError> {
    ctx.check_cancelled()?;

    let mut executed_steps = Vec::with_capacity(step_ids.len());

    for step_id in step_ids {
        let step_id = step_id.as_ref();

        // 1. Hard cancellation check takes priority over soft stop (Conventions.md §6.3).
        ctx.check_cancelled()?;

        // 2. Soft stop observed strictly between steps (Conventions.md §6.2).
        if let Some(rx) = soft_stop_rx {
            if *rx.borrow() {
                return Ok(LoopResult::Success(LoopSuccess::new(
                    PlanSequenceArtifact {
                        executed_steps,
                        stop: PlanSequenceStop::Stopped,
                    },
                    ResponseEnvelope::default(),
                )));
            }
        }

        // 3. Execute individual step.
        let step_result = run_plan_step_loop_with(ctx, persistence, step_id, &config.step).await?;
        match step_result {
            LoopResult::Success(success) => {
                executed_steps.push(success.value);
            }
            LoopResult::Diverged(envelope) => {
                return Ok(LoopResult::Diverged(envelope));
            }
            LoopResult::Exhausted(report) => {
                return Ok(LoopResult::Exhausted(report));
            }
        }
    }

    Ok(LoopResult::Success(LoopSuccess::new(
        PlanSequenceArtifact {
            executed_steps,
            stop: PlanSequenceStop::Completed,
        },
        ResponseEnvelope::default(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::review::ReviewStorage;

    use tauqe_protocol::{
        CapabilityExpr, ConceptItem, EditProtocolKind, LlmCapability, ModelRef, Plan,
        PlanImplementation, PlanItem, PlanItemStatus, ReviewGroup, ReviewSession,
    };

    use crate::config::ModelsConfig;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink, LoopContext};
    use crate::control::prompt::PromptBuilder;
    use crate::control::turn::TurnLoopConfig;
    use crate::plan::storage::PlanStorage;
    use crate::prompt::json::JsonPromptProtocol;
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

    fn test_config() -> PlanSequenceLoopConfig {
        PlanSequenceLoopConfig {
            step: PlanStepLoopConfig {
                turn: TurnLoopConfig::default(),
                models: ModelsConfig::default(),
                protocol: EditProtocolKind::Structured,
                marker: None,
                selection: None,
            },
        }
    }

    fn stored_two_step_plan(root: &Path, id: &str) -> Plan {
        let plan = Plan::from_parts(
            id.to_string(),
            "Sequence Plan".to_string(),
            Some("A test plan for sequence loop".to_string()),
            0,
            0,
            vec![ConceptItem {
                id: "c-1".to_string(),
                title: "Concept".to_string(),
                details: None,
                children: Vec::new(),
            }],
            vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Step 1".to_string(),
                    status: PlanItemStatus::Todo,
                    files: vec!["one.rs".to_string()],
                    ..PlanItem::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Step 2".to_string(),
                    status: PlanItemStatus::Todo,
                    files: vec!["two.rs".to_string()],
                    ..PlanItem::default()
                },
            ],
        );
        PlanStorage::save_plan(root, &plan).expect("save");
        plan
    }

    #[tokio::test]
    async fn sequence_loop_executes_all_steps_successfully() {
        let mut fixture = Fixture::new(
            &[
                (
                    "one.rs",
                    "fn one() {}
",
                ),
                (
                    "two.rs",
                    "fn two() {}
",
                ),
            ],
            vec![
                MockResponse::text(
                    serde_json::json!({
                        "message": "Update one",
                        "action": {
                            "kind": "edit",
                            "changes": [{
                                "op": "replace",
                                "path": "one.rs",
                                "to": "",
                                "start": "",
                                "end": "",
                                "content": "",
                                "old_text": "fn one() {}\n",
                                "new_text": "fn one() -> i32 { 1 }\n"
                            }],
                            "commit_message": "feat(one): update one",
                            "message": "",
                            "reason": ""
                        }
                    })
                    .to_string(),
                ),
                MockResponse::text(
                    serde_json::json!({
                        "message": "Update two",
                        "action": {
                            "kind": "edit",
                            "changes": [{
                                "op": "replace",
                                "path": "two.rs",
                                "to": "",
                                "start": "",
                                "end": "",
                                "content": "",
                                "old_text": "fn two() {}\n",
                                "new_text": "fn two() -> i32 { 2 }\n"
                            }],
                            "commit_message": "feat(two): update two",
                            "message": "",
                            "reason": ""
                        }
                    })
                    .to_string(),
                ),
            ],
        );
        let root = fixture.dir.path().to_path_buf();
        stored_two_step_plan(&root, "seq-plan");

        let mut ctx = fixture.context();
        let result =
            run_plan_sequence_loop(&mut ctx, "seq-plan", &["1", "2"], &test_config(), None)
                .await
                .expect("sequence loop succeeds");

        assert!(result.is_success());
        let success = result.into_success().expect("success");
        assert_eq!(success.value.stop, PlanSequenceStop::Completed);
        assert_eq!(success.value.executed_steps.len(), 2);
        assert_eq!(success.value.executed_steps[0].step_id, "1");
        assert_eq!(success.value.executed_steps[1].step_id, "2");
    }

    #[tokio::test]
    async fn sequence_loop_stops_on_soft_stop_between_steps() {
        let mut fixture = Fixture::new(
            &[
                (
                    "one.rs",
                    "fn one() {}
",
                ),
                (
                    "two.rs",
                    "fn two() {}
",
                ),
            ],
            vec![MockResponse::text(
                serde_json::json!({
                    "message": "Update one",
                    "action": {
                        "kind": "edit",
                        "changes": [{
                            "op": "replace",
                            "path": "one.rs",
                            "to": "",
                            "start": "",
                            "end": "",
                            "content": "",
                            "old_text": "fn one() {}\n",
                            "new_text": "fn one() -> i32 { 1 }\n"
                        }],
                        "commit_message": "feat(one): update one",
                        "message": "",
                        "reason": ""
                    }
                })
                .to_string(),
            )],
        );
        let root = fixture.dir.path().to_path_buf();
        stored_two_step_plan(&root, "seq-plan");

        let (soft_stop_tx, soft_stop_rx) = watch::channel(false);
        let mut ctx = fixture.context();

        // Trigger soft stop before the second step executes.
        soft_stop_tx.send(true).expect("send soft stop");

        let result = run_plan_sequence_loop(
            &mut ctx,
            "seq-plan",
            &["1", "2"],
            &test_config(),
            Some(&soft_stop_rx),
        )
        .await
        .expect("sequence loop finishes");

        assert!(result.is_success());
        let success = result.into_success().expect("success");
        assert_eq!(success.value.stop, PlanSequenceStop::Stopped);
        assert!(success.value.executed_steps.is_empty());
    }

    #[tokio::test]
    async fn sequence_loop_honors_hard_cancellation() {
        let mut fixture = Fixture::new(&[], vec![]);
        let root = fixture.dir.path().to_path_buf();
        stored_two_step_plan(&root, "seq-plan");

        let mut ctx = fixture.context();
        ctx.cancel.cancel();

        let err = run_plan_sequence_loop(&mut ctx, "seq-plan", &["1", "2"], &test_config(), None)
            .await
            .expect_err("must be cancelled");

        assert!(matches!(err, SystemError::Cancelled));
    }

    fn stored_review_group(root: &Path, review_id: &str, group_id: &str) {
        let session = ReviewSession {
            id: review_id.to_string(),
            title: "Review".to_string(),
            created_at: 1,
            groups: vec![ReviewGroup {
                id: group_id.to_string(),
                title: "Bound the emit length".to_string(),
                description: Some("One shared root cause".to_string()),
                implementation: PlanImplementation {
                    items: vec![
                        PlanItem {
                            id: "1".to_string(),
                            title: "Step 1".to_string(),
                            files: vec!["one.rs".to_string()],
                            ..PlanItem::default()
                        },
                        PlanItem {
                            id: "2".to_string(),
                            title: "Step 2".to_string(),
                            files: vec!["two.rs".to_string()],
                            ..PlanItem::default()
                        },
                    ],
                    ..PlanImplementation::default()
                },
                ..ReviewGroup::default()
            }],
            ..ReviewSession::default()
        };
        ReviewStorage::save_session(root, &session).expect("save review session");
    }

    #[tokio::test]
    async fn review_group_horizon_runs_through_the_sequence_loop() {
        let mut fixture = Fixture::new(
            &[("one.rs", "fn one() {}\n"), ("two.rs", "fn two() {}\n")],
            vec![
                MockResponse::text(
                    serde_json::json!({
                        "message": "Update one",
                        "action": {
                            "kind": "edit",
                            "changes": [{
                                "op": "replace",
                                "path": "one.rs",
                                "to": "",
                                "start": "",
                                "end": "",
                                "content": "",
                                "old_text": "fn one() {}\n",
                                "new_text": "fn one() -> i32 { 1 }\n"
                            }],
                            "commit_message": "feat(one): update one",
                            "message": "",
                            "reason": ""
                        }
                    })
                    .to_string(),
                ),
                MockResponse::text(
                    serde_json::json!({
                        "message": "Update two",
                        "action": {
                            "kind": "edit",
                            "changes": [{
                                "op": "replace",
                                "path": "two.rs",
                                "to": "",
                                "start": "",
                                "end": "",
                                "content": "",
                                "old_text": "fn two() {}\n",
                                "new_text": "fn two() -> i32 { 2 }\n"
                            }],
                            "commit_message": "feat(two): update two",
                            "message": "",
                            "reason": ""
                        }
                    })
                    .to_string(),
                ),
            ],
        );
        let root = fixture.dir.path().to_path_buf();
        stored_review_group(&root, "review-1", "group-1");

        let persistence = ReviewGroupPersistence::new("review-1", "group-1");
        let mut ctx = fixture.context();
        let result =
            run_plan_sequence_loop_with(&mut ctx, &persistence, &["1", "2"], &test_config(), None)
                .await
                .expect("sequence loop succeeds");

        assert!(result.is_success());
        let success = result.into_success().expect("success");
        assert_eq!(success.value.executed_steps.len(), 2);

        // The lifecycle of the group horizon is durable in the review session:
        // both steps reached Done together with the commit that produced them.
        let session = ReviewStorage::load_review(&root, "review-1")
            .expect("load review")
            .expect("review exists");
        let group = session
            .groups
            .iter()
            .find(|group| group.id == "group-1")
            .expect("group exists");
        assert_eq!(group.implementation.items[0].status, PlanItemStatus::Done);
        assert!(group.implementation.items[0].commit.is_some());
        assert_eq!(group.implementation.items[1].status, PlanItemStatus::Done);
        assert!(group.implementation.items[1].commit.is_some());
    }
}
