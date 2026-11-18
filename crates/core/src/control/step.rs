//! Execute single and sequential step bodies through the bounded control loops.

use crate::config::AppConfig;
use crate::context::ContextManager;
use crate::control::context::{CancellationToken, EventSink, LoopContext};
use crate::control::outcome::{LoopResult, SystemError};
use crate::control::plan::sequence::{
    run_plan_sequence_loop_with, PlanSequenceArtifact, PlanSequenceLoopConfig,
    ReviewMemberPersistence,
};
use crate::control::plan::step::PlanStepLoopConfig;
use crate::control::prompt::PromptBuilder;
use crate::control::turn::{run_turn_loop, TurnArtifact, TurnLoopConfig};
use crate::models::{ModelRef, ModelSelection, ReasoningEffort};
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::TurnBrief;
use crate::providers::LlmProvider;
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::research::journal::TurnResearchJournal;
use std::path::Path;

/// Which run body a step endpoint executes through the control contour.
pub enum StepLoopBody {
    /// One bounded turn assembled from the prompt the caller prepared.
    Single(String),
    /// An ordered sequence of the steps a review horizon made ready.
    Sequence {
        /// Document that records the executed steps.
        persistence: ReviewMemberPersistence,
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
    pub repo_root: &'a Path,
    pub provider: &'a dyn LlmProvider,
    pub model: &'a ModelRef,
    pub protocol: &'a dyn PromptProtocol,
    pub config: &'a AppConfig,
    pub selection: &'a ModelSelection,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub marker: Option<String>,
    pub session_id: Option<String>,
}

pub async fn run_step_execution(
    context_manager: &mut ContextManager,
    policy: &mut dyn ApplyPolicy,
    cancel: &CancellationToken,
    sink: &EventSink,
    execution: StepExecutionConfig<'_>,
    body: StepLoopBody,
) -> StepLoopReport {
    let mut journal = TurnResearchJournal::default();
    let mut input_cache = crate::control::cache::TurnInputCache::default();
    let capabilities = crate::control::turn::turn_capability_expr();
    let prompt = match &body {
        StepLoopBody::Single(prompt) => Some(prompt.clone()),
        StepLoopBody::Sequence { .. } => None,
    };
    let turn_config = TurnLoopConfig::from_app_config(execution.config);
    let sequence_config = PlanSequenceLoopConfig {
        step: PlanStepLoopConfig {
            turn: turn_config.clone(),
            models: execution.config.models.clone(),
            protocol: crate::workflow::EditProtocolKind::Structured,
            marker: execution.marker,
            selection: Some(execution.selection.clone()),
        },
    };
    let prompt_builder = PromptBuilder::new(
        execution.provider,
        execution.model,
        execution.protocol,
        &mut input_cache,
    )
    .with_session_id(execution.session_id)
    .with_prompt(prompt)
    .with_history(None)
    .with_repo_map_config(
        execution.config.context.include_repo_map,
        execution.config.context.repomap_token_budget,
    )
    .with_patch_strategy(execution.config.repl.patch_strategy)
    .with_workflow(execution.config.repl.workflow.to_string())
    .with_outer_docs(execution.config.outer_docs.clone())
    .with_turn_brief(Some(TurnBrief::step()))
    .with_reasoning_effort(execution.reasoning_effort);
    let mut ctx = LoopContext::new(
        cancel,
        sink,
        execution.repo_root,
        context_manager,
        &mut journal,
        policy,
        capabilities,
        prompt_builder,
    );
    match body {
        StepLoopBody::Single(_) => match run_turn_loop(&mut ctx, None, &turn_config).await {
            Ok(result) => StepLoopReport::Turn(result),
            Err(SystemError::Cancelled) => StepLoopReport::Cancelled,
            Err(err) => StepLoopReport::Failed(err.to_string()),
        },
        StepLoopBody::Sequence { persistence, steps } => {
            match run_plan_sequence_loop_with(
                &mut ctx,
                &persistence,
                &steps,
                &sequence_config,
                None,
            )
            .await
            {
                Ok(result) => StepLoopReport::Sequence(result),
                Err(SystemError::Cancelled) => StepLoopReport::Cancelled,
                Err(err) => StepLoopReport::Failed(err.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::implementation::ExecutionScope;
    use crate::workflow::policy::DirectApplyPolicy;

    struct Fixture {
        root: tempfile::TempDir,
        context: ContextManager,
        provider: MockLlmProvider,
        cancel: CancellationToken,
    }

    impl Fixture {
        fn new(responses: Vec<MockResponse>) -> Self {
            let root = tempfile::tempdir().unwrap();
            crate::git::init_repository(Some(root.path()), true).unwrap();
            Self {
                context: ContextManager::new(root.path().to_path_buf()),
                root,
                provider: MockLlmProvider::scripted(responses),
                cancel: CancellationToken::new(),
            }
        }

        async fn run(&mut self, body: StepLoopBody) -> StepLoopReport {
            let mut config = AppConfig::default();
            config.context.include_repo_map = false;
            let model = ModelRef::openrouter("mock/model");
            let selection = ModelSelection::Specific(model.clone());
            let protocol = JsonPromptProtocol::edits();
            let mut policy = DirectApplyPolicy::new();
            run_step_execution(
                &mut self.context,
                &mut policy,
                &self.cancel,
                &EventSink::noop(),
                StepExecutionConfig {
                    repo_root: self.root.path(),
                    provider: &self.provider,
                    model: &model,
                    protocol: &protocol,
                    config: &config,
                    selection: &selection,
                    reasoning_effort: Some(ReasoningEffort::High),
                    marker: None,
                    session_id: Some("step-session".into()),
                },
                body,
            )
            .await
        }
    }

    fn sequence(steps: Vec<String>) -> StepLoopBody {
        StepLoopBody::Sequence {
            persistence: ReviewMemberPersistence::of(&ExecutionScope::ReviewFinding {
                review_id: "review".into(),
                item_id: 7,
            })
            .unwrap(),
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
            fixture.cancel.cancel();
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
}
