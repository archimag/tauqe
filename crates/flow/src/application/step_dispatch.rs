//! Execute single and sequential step bodies through the bounded control loops.

use tauqe_core::context::ContextManager;
use tauqe_core::control::context::{CancellationToken, EventSink};
use tauqe_core::control::execution::{TurnExecutionConfig, TurnInputs};
use tauqe_core::control::outcome::{LoopResult, SystemError};
use tauqe_core::control::plan::sequence::{
    run_plan_sequence_loop_with, PlanSequenceArtifact, PlanSequenceLoopConfig,
};
use tauqe_core::control::plan::step::{PlanStepLoopConfig, StepPersistence};
use tauqe_core::control::step::run_step_execution;
use tauqe_core::control::turn::{TurnArtifact, TurnLoopConfig};
use tauqe_core::models::ModelSelection;
use tauqe_core::workflow::policy::ApplyPolicy;

/// Which run body a step endpoint executes through the control contour.
pub enum StepLoopBody {
    /// One bounded turn assembled from the prompt the caller prepared.
    Single(String),
    /// An ordered sequence of the steps a review horizon made ready.
    Sequence {
        /// Document that records the executed steps.
        persistence: Box<dyn StepPersistence>,
        /// Identifiers of the steps to execute, in dependency order.
        steps: Vec<String>,
    },
}

/// Terminal report of one step-endpoint execution.
pub enum StepLoopReport {
    /// A single bounded turn reached its terminal result.
    Turn(LoopResult<TurnArtifact>),
    /// A sequence of plan steps reached its terminal result.
    Sequence(LoopResult<PlanSequenceArtifact>),
    /// The run observed a cancellation request.
    Cancelled,
    /// The run could not be started or failed outside the domain outcome.
    Failed(String),
}

/// Resolved dependencies and frozen configuration of one step execution.
pub struct StepExecutionConfig<'a> {
    pub turn: TurnExecutionConfig<'a>,
    pub selection: &'a ModelSelection,
    pub marker: Option<String>,
}

pub(crate) async fn dispatch_step_execution(
    context_manager: &mut ContextManager,
    policy: &mut dyn ApplyPolicy,
    cancel: &CancellationToken,
    sink: &EventSink,
    execution: StepExecutionConfig<'_>,
    body: StepLoopBody,
) -> StepLoopReport {
    let outcome = match body {
        StepLoopBody::Single(prompt) => run_step_execution(
            context_manager,
            policy,
            cancel,
            sink,
            execution.turn,
            prompt,
        )
        .await
        .map(StepLoopReport::Turn),
        StepLoopBody::Sequence { persistence, steps } => {
            let config = execution.turn.config;
            let turn_config = TurnLoopConfig::from_app_config(config);
            let mut inputs = TurnInputs::default();
            let mut ctx =
                inputs.prepare_step(execution.turn, None, context_manager, policy, cancel, sink);
            let sequence_config = PlanSequenceLoopConfig {
                step: PlanStepLoopConfig {
                    turn: turn_config,
                    models: config.models.clone(),
                    protocol: tauqe_core::workflow::EditProtocolKind::Structured,
                    marker: execution.marker,
                    selection: Some(execution.selection.clone()),
                },
            };
            run_plan_sequence_loop_with(
                &mut ctx,
                persistence.as_ref(),
                &steps,
                &sequence_config,
                None,
            )
            .await
            .map(StepLoopReport::Sequence)
        }
    };
    match outcome {
        Ok(report) => report,
        Err(SystemError::Cancelled) => StepLoopReport::Cancelled,
        Err(error) => StepLoopReport::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::review_batch::ReviewMemberPersistence;
    use std::path::Path;
    use tauqe_core::plan::{PlanImplementation, PlanItem, PlanItemStatus};
    use tauqe_core::prompt::json::JsonPromptProtocol;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::review::{ReviewGroup, ReviewSession, ReviewStorage};
    use tauqe_core::workflow::git_policy::GitTransactionPolicy;
    use tauqe_core::workflow::implementation::ExecutionScope;
    use tauqe_core::{
        config::AppConfig,
        models::{ModelRef, ReasoningEffort},
    };

    struct Fixture {
        root: tempfile::TempDir,
        context: ContextManager,
        provider: MockLlmProvider,
        cancel: CancellationToken,
        config: AppConfig,
    }

    impl Fixture {
        fn new(responses: Vec<MockResponse>) -> Self {
            let root = tempfile::tempdir().unwrap();
            tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
            Self {
                context: ContextManager::new(root.path().to_path_buf()),
                root,
                provider: MockLlmProvider::scripted(responses),
                cancel: CancellationToken::new(),
                config: AppConfig::default(),
            }
        }

        async fn run(&mut self, body: StepLoopBody) -> StepLoopReport {
            self.config.context.include_repo_map = false;
            let model = ModelRef::openrouter("mock/model");
            let selection = ModelSelection::Specific(model.clone());
            let protocol =
                JsonPromptProtocol::new(tauqe_core::control::turn::turn_capability_expr());
            let mut policy = GitTransactionPolicy::new();
            dispatch_step_execution(
                &mut self.context,
                &mut policy,
                &self.cancel,
                &EventSink::noop(),
                StepExecutionConfig {
                    turn: tauqe_core::control::execution::TurnExecutionConfig {
                        repo_root: self.root.path(),
                        provider: &self.provider,
                        model: &model,
                        protocol: &protocol,
                        config: &self.config,
                        reasoning_effort: Some(ReasoningEffort::High),
                        session_id: Some("step-session".into()),
                    },
                    selection: &selection,
                    marker: None,
                },
                body,
            )
            .await
        }
    }

    fn sequence(steps: Vec<String>) -> StepLoopBody {
        StepLoopBody::Sequence {
            persistence: Box::new(
                ReviewMemberPersistence::of(&ExecutionScope::ReviewFinding {
                    review_id: "review".into(),
                    item_id: 7,
                })
                .unwrap(),
            ),
            steps,
        }
    }

    #[tokio::test]
    async fn single_execution_forwards_prompt_session_and_effort_to_the_provider() {
        let mut fixture = Fixture::new(vec![MockResponse::text(serde_json::json!({
            "message": "", "action": { "kind": "edit", "changes": [], "commit_message": "", "message": "", "reason": "Already done" }
        }).to_string())]);
        let report = fixture
            .run(StepLoopBody::Single("Execute this step".into()))
            .await;
        assert!(matches!(
            report,
            StepLoopReport::Turn(LoopResult::Success(_))
        ));
        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].session_id.as_deref(), Some("step-session"));
        assert_eq!(requests[0].reasoning_effort, Some(ReasoningEffort::High));
        assert!(requests[0]
            .messages
            .iter()
            .any(|message| message.content.contains("Execute this step")));
    }

    #[tokio::test]
    async fn provider_failure_is_reported_without_becoming_a_domain_outcome() {
        let mut fixture = Fixture::new(vec![MockResponse::error("provider unavailable")]);
        let report = fixture
            .run(StepLoopBody::Single("Execute step".into()))
            .await;
        assert!(
            matches!(report, StepLoopReport::Failed(reason) if reason.contains("provider unavailable"))
        );
        assert_eq!(fixture.provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn cancelled_single_and_sequence_runs_never_call_the_provider() {
        for body in [
            StepLoopBody::Single("Execute step".into()),
            sequence(vec!["1".into()]),
        ] {
            let mut fixture = Fixture::new(Vec::new());
            let (_sender, cancelled) = tokio::sync::watch::channel(true);
            fixture.cancel = CancellationToken::from_receiver(cancelled);
            assert!(matches!(fixture.run(body).await, StepLoopReport::Cancelled));
            assert!(fixture.provider.requests().is_empty());
        }
    }

    #[tokio::test]
    async fn empty_sequence_finishes_without_loading_a_review_or_calling_a_provider() {
        let mut fixture = Fixture::new(Vec::new());
        match fixture.run(sequence(Vec::new())).await {
            StepLoopReport::Sequence(LoopResult::Success(success)) => {
                assert!(success.value.executed_steps.is_empty());
                assert!(success.value.is_success());
            }
            _ => panic!("an empty sequence must complete successfully"),
        }
        assert!(fixture.provider.requests().is_empty());
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
        let mut fixture = Fixture::new(vec![
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
        ]);
        fixture.config =
            toml::from_str("[repl]\nmax_turn_rounds = 1\npatch_strategy = 'line_anchor'").unwrap();
        let root = fixture.root.path().to_path_buf();
        for (name, source) in [("one.rs", "fn one() {}\n"), ("two.rs", "fn two() {}\n")] {
            std::fs::write(root.join(name), source).unwrap();
        }
        tauqe_core::git::cmd::stage_files(&root, &["one.rs".into(), "two.rs".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(&root, "Initial sources", false)
            .unwrap();
        stored_review_group(&root, "review-1", "group-1");

        let persistence = crate::application::review_batch::ReviewMemberPersistence::of(
            &tauqe_core::workflow::implementation::ExecutionScope::ReviewGroup {
                review_id: "review-1".into(),
                group_id: "group-1".into(),
            },
        )
        .unwrap();
        let report = fixture
            .run(StepLoopBody::Sequence {
                persistence: Box::new(persistence),
                steps: vec!["1".into(), "2".into()],
            })
            .await;
        let StepLoopReport::Sequence(LoopResult::Success(success)) = report else {
            panic!("review group sequence must complete");
        };
        assert_eq!(success.value.executed_steps.len(), 2);

        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 2);
        for (index, request) in requests.iter().enumerate() {
            let task = format!("[review-1:{}] Step {}", index + 1, index + 1);
            assert!(request
                .messages
                .iter()
                .any(|message| message.content.contains(&task)));
        }

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
