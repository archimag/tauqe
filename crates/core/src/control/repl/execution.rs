//! Assemble and execute a turn-local interactive control context.

use super::{ReplArtifact, ReplLoopConfig};
use crate::config::AppConfig;
use crate::context::ContextManager;
use crate::control::context::{CancellationToken, EventSink, LoopContext};
use crate::control::outcome::{LoopResult, SystemError};
use crate::control::prompt::PromptBuilder;
use crate::discussion::DiscussionTarget;
use crate::history::HistorySnapshot;
use crate::models::{ModelRef, ReasoningEffort};
use crate::prompt::protocol::PromptProtocol;
use crate::providers::LlmProvider;
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::research::journal::TurnResearchJournal;
use std::path::Path;

/// Resolved dependencies and frozen prompt inputs of one interactive execution.
pub struct ReplExecutionConfig<'a> {
    pub repo_root: &'a Path,
    pub provider: &'a dyn LlmProvider,
    pub model: &'a ModelRef,
    pub protocol: &'a dyn PromptProtocol,
    pub config: &'a AppConfig,
    pub prompt: String,
    pub history: Option<&'a HistorySnapshot>,
    pub focus: Option<&'a DiscussionTarget>,
    pub session_id: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

pub async fn run_repl_execution(
    context_manager: &mut ContextManager,
    policy: &mut dyn ApplyPolicy,
    cancel: &CancellationToken,
    sink: &EventSink,
    execution: ReplExecutionConfig<'_>,
) -> Result<LoopResult<ReplArtifact>, SystemError> {
    let mut journal = TurnResearchJournal::default();
    let mut input_cache = crate::control::cache::TurnInputCache::default();
    let capabilities = super::repl_capability_expr(execution.focus);
    let prompt_builder = PromptBuilder::new(
        execution.provider,
        execution.model,
        execution.protocol,
        &mut input_cache,
    )
    .with_session_id(execution.session_id)
    .with_prompt(Some(execution.prompt))
    .with_history(execution.history)
    .with_repo_map_config(
        execution.config.context.include_repo_map,
        execution.config.context.repomap_token_budget,
    )
    .with_patch_strategy(execution.config.repl.patch_strategy)
    .with_workflow(execution.config.repl.workflow.to_string())
    .with_outer_docs(execution.config.outer_docs.clone())
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
    super::run_repl_loop(
        &mut ctx,
        &ReplLoopConfig::from_app_config(execution.config),
        execution.focus,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryManager;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
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

        async fn run(
            &mut self,
            history: Option<&HistorySnapshot>,
            focus: Option<&DiscussionTarget>,
        ) -> Result<LoopResult<ReplArtifact>, SystemError> {
            let mut config = AppConfig::default();
            config.context.include_repo_map = false;
            let protocol = JsonPromptProtocol::edits()
                .with_capability_expr(crate::control::repl_capabilities(focus));
            let model = ModelRef::openrouter("mock/model");
            let mut policy = DirectApplyPolicy::new();
            run_repl_execution(
                &mut self.context,
                &mut policy,
                &self.cancel,
                &EventSink::noop(),
                ReplExecutionConfig {
                    repo_root: self.root.path(),
                    provider: &self.provider,
                    model: &model,
                    protocol: protocol.as_ref(),
                    config: &config,
                    prompt: "Current developer request".into(),
                    history,
                    focus,
                    session_id: Some("interactive-session".into()),
                    reasoning_effort: Some(ReasoningEffort::High),
                },
            )
            .await
        }
    }

    #[tokio::test]
    async fn execution_keeps_frozen_history_prompt_session_and_effort() {
        let mut fixture = Fixture::new(vec![MockResponse::text("Interactive reply")]);
        let mut history = HistoryManager::new(fixture.root.path().to_path_buf());
        history
            .record_turn("Historical request", Vec::new())
            .unwrap();
        history
            .record_response("Historical answer", None, None, Vec::new())
            .unwrap();
        let snapshot = history.snapshot().unwrap();
        history
            .record_turn("Newer live history", Vec::new())
            .unwrap();
        let result = fixture.run(Some(&snapshot), None).await.unwrap();
        match result {
            LoopResult::Success(success) => {
                assert_eq!(success.value.message.as_deref(), Some("Interactive reply"))
            }
            _ => panic!("interactive execution must return a completed reply"),
        }
        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.session_id.as_deref(), Some("interactive-session"));
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
        assert!(request
            .messages
            .iter()
            .any(|message| message.content.contains("Current developer request")));
        assert!(request
            .messages
            .iter()
            .any(|message| message.content.contains("Historical request")));
        assert!(request
            .messages
            .iter()
            .all(|message| !message.content.contains("Newer live history")));
    }

    #[tokio::test]
    async fn focused_execution_keeps_the_declared_plan_or_review_surface() {
        for (focus, allowed, refused) in [
            (
                DiscussionTarget::Plan { id: "plan".into() },
                "plan_update",
                "review_update",
            ),
            (
                DiscussionTarget::Review {
                    id: "review".into(),
                },
                "review_update",
                "plan_update",
            ),
        ] {
            let mut fixture = Fixture::new(vec![MockResponse::text("Focused reply")]);
            assert!(matches!(
                fixture.run(None, Some(&focus)).await.unwrap(),
                LoopResult::Success(_)
            ));
            let requests = fixture.provider.requests();
            let schema = serde_json::to_string(&requests[0].response_format).unwrap();
            assert!(schema.contains(&format!("\"{allowed}\"")));
            assert!(!schema.contains(&format!("\"{refused}\"")));
        }
    }

    #[tokio::test]
    async fn cancellation_before_execution_never_calls_the_provider() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.cancel.cancel();
        assert!(matches!(
            fixture.run(None, None).await,
            Err(SystemError::Cancelled)
        ));
        assert!(fixture.provider.requests().is_empty());
    }

    #[tokio::test]
    async fn provider_failure_remains_an_infrastructure_error() {
        let mut fixture = Fixture::new(vec![MockResponse::error("provider unavailable")]);
        assert!(
            matches!(fixture.run(None, None).await, Err(SystemError::Provider(reason)) if reason.contains("provider unavailable"))
        );
        assert_eq!(fixture.provider.requests().len(), 1);
    }
}
