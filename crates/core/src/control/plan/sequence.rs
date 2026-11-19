//! Plan sequence execution loop.
//!
//! [`run_plan_sequence_loop`] drives an ordered sequence of plan steps
//! (such as a review group subplan, a range of steps, or a linear plan)
//! through [`run_plan_step_loop_with`], observing soft-stop and cancellation
//! signals between steps. The persistence adapter decides which document records
//! the lifecycle, so a local plan and a review group horizon share one loop.

use crate::control::execution::TurnInputs;
use crate::workflow::batch::QueueInterruption;
use tokio::sync::watch;

use crate::control::context::LoopContext;
use crate::control::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use crate::control::plan::step::{
    run_plan_step_loop_with, PlanStepArtifact, PlanStepLoopConfig, StepPersistence,
};
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
    fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
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
#[cfg(test)]
pub(crate) async fn run_plan_sequence_loop<S: AsRef<str>>(
    ctx: &mut LoopContext<'_>,
    plan_id: &str,
    step_ids: &[S],
    config: &PlanSequenceLoopConfig,
    soft_stop_rx: Option<&watch::Receiver<bool>>,
) -> Result<LoopResult<PlanSequenceArtifact>, SystemError> {
    let persistence = super::step::PlanDocumentPersistence::new(plan_id);
    run_plan_sequence_loop_with(ctx, &persistence, step_ids, config, soft_stop_rx).await
}

/// Drives an ordered sequence of steps recorded by any document.
///
/// The sequence semantics belong to this loop alone: a local plan and a review
/// group horizon differ only in the adapter that loads and persists the steps, so
/// both observe the same soft-stop boundary and the same cancellation precedence.
pub async fn run_plan_sequence_loop_with<S: AsRef<str>>(
    ctx: &mut LoopContext<'_>,
    persistence: &dyn StepPersistence,
    step_ids: &[S],
    config: &PlanSequenceLoopConfig,
    soft_stop_rx: Option<&watch::Receiver<bool>>,
) -> Result<LoopResult<PlanSequenceArtifact>, SystemError> {
    ctx.check_cancelled()?;

    let mut executed_steps = Vec::with_capacity(step_ids.len());

    for step_id in step_ids {
        let step_id = step_id.as_ref();

        match QueueInterruption::before_next(ctx.cancel.is_cancelled(), soft_stop_rx) {
            Some(QueueInterruption::Cancelled) => return Err(SystemError::Cancelled),
            Some(QueueInterruption::Stopped) => {
                tracing::info!(repo_root = %ctx.repo_root.display(), executed = executed_steps.len(),
                    remaining = step_ids.len().saturating_sub(executed_steps.len()), "plan sequence stopped between steps");
                return Ok(LoopResult::Success(LoopSuccess::new(
                    PlanSequenceArtifact {
                        executed_steps,
                        stop: PlanSequenceStop::Stopped,
                    },
                    ResponseEnvelope::default(),
                )));
            }
            None => {}
        }

        let mut inputs = TurnInputs::default();
        let mut member = inputs.prepare_member(ctx);
        let step_result =
            run_plan_step_loop_with(&mut member, persistence, step_id, &config.step).await?;
        match step_result {
            LoopResult::Success(success) => {
                executed_steps.push(success.value);
            }
            LoopResult::Diverged(envelope) => {
                tracing::warn!(
                    repo_root = %ctx.repo_root.display(),
                    step_id,
                    "plan sequence diverged on a step"
                );
                return Ok(LoopResult::Diverged(envelope));
            }
            LoopResult::Exhausted(report) => {
                tracing::warn!(
                    repo_root = %ctx.repo_root.display(),
                    step_id,
                    attempts = report.attempts,
                    reason = %report.reason,
                    "plan sequence exhausted the retry budget of a step"
                );
                return Ok(LoopResult::Exhausted(report));
            }
        }
    }

    tracing::info!(
        repo_root = %ctx.repo_root.display(),
        steps = executed_steps.len(),
        "plan sequence completed"
    );
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

    use crate::models::ModelRef;
    use crate::plan::{ConceptItem, Plan, PlanItem, PlanItemStatus};
    use crate::workflow::EditProtocolKind;

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
                protocol: JsonPromptProtocol::new(crate::control::turn::turn_capability_expr()),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::git_policy::GitTransactionPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = crate::control::turn::turn_capability_expr();
            let prompt_builder =
                PromptBuilder::new(&self.provider, &self.model, &self.protocol, &mut self.cache)
                    .with_patch_strategy(crate::workflow::PatchStrategy::LineAnchor);
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
}
