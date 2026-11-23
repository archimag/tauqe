//! Technical execution dependencies frozen for one application operation.

use crate::api::{Application, OperationGuard};
use crate::application::configuration::ConfigurationSnapshot;
use crate::application::operation::OperationRegistry;
use crate::application::turn_setup::{ResolvedTurnModel, TurnProviderError};
use crate::workflows::architect::ArchitectScope;
use std::sync::Arc;
use tauqe_core::execution::structured::{self, StructuredContract, StructuredError};
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptExecutor, PromptInput,
};
use tauqe_core::llm::CapabilityTree;
use tauqe_core::models::{
    ArchitectTier, ModelRef, ModelRole, ModelSelection, ProviderKind, ReasoningEffort,
};
use tauqe_core::prompt::refine::{PlanRefinePromptProtocol, PlanRefineRound};
use tauqe_core::prompt::{json::JsonPromptProtocol, protocol::PromptProtocol};
use tauqe_core::providers::{
    format_session_id, ChatMessage, LlmProvider, SESSION_PREFIX_ARCHITECT, SESSION_PREFIX_REVIEW,
};
use tauqe_core::workflow::policy::{ApplyPolicy, DirectApplyPolicy};
use tauqe_core::workflow::WorkflowKind;
use tauqe_core::workflow::{PatchStrategy, WorkflowOptions};
use tokio::sync::watch;

#[cfg(any(test, feature = "test-support"))]
pub(crate) type ProviderOverrides =
    Arc<tokio::sync::Mutex<std::collections::HashMap<ProviderKind, Arc<dyn LlmProvider>>>>;

/// Session resources needed to bind technical execution without the facade.
#[derive(Clone)]
pub(crate) struct ExecutionSession {
    pub operations: Arc<OperationRegistry>,
    pub transaction: Arc<std::sync::atomic::AtomicBool>,
    pub login: Option<Arc<tauqe_core::providers::ChatGptSignIn>>,
    #[cfg(any(test, feature = "test-support"))]
    pub provider_override: ProviderOverrides,
}

impl ExecutionSession {
    pub(crate) fn owns(&self, operation: &OperationGuard) -> bool {
        operation.belongs_to(&self.operations)
    }

    fn policy(
        &self,
        workflow: WorkflowKind,
        cancel: watch::Receiver<bool>,
    ) -> Box<dyn ApplyPolicy> {
        match workflow {
            WorkflowKind::Naive => Box::new(DirectApplyPolicy::new()),
            WorkflowKind::Git => Box::new(
                tauqe_core::workflow::git_policy::GitTransactionPolicy::with_guard_and_cancel(
                    self.transaction.clone(),
                    cancel,
                ),
            ),
        }
    }

    async fn provider(
        &self,
        config: &tauqe_core::config::AppConfig,
        kind: ProviderKind,
    ) -> Result<Arc<dyn LlmProvider>, TurnProviderError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(provider) = self.provider_override.lock().await.get(&kind).cloned() {
            return Ok(provider);
        }
        tauqe_core::providers::create_provider_with_login(kind, config, self.login.clone())
            .map(Arc::from)
            .map_err(|error| TurnProviderError::new(error.to_string()))
    }
}

/// Frozen environment of an admitted operation. The operation is checked again
/// at resolution time so a stale guard cannot start work under its successor.
pub(crate) struct ExecutionFactory<'a> {
    application: &'a Application,
    operation: &'a OperationGuard,
    environment: ExecutionEnvironment,
}

pub(crate) struct ExecutionEnvironment {
    snapshot: ConfigurationSnapshot,
}

/// Frozen journal policy and operation-bound Scribe execution.
#[derive(Clone)]
pub(crate) struct CompactionEnvironment {
    environment: Arc<ExecutionEnvironment>,
    operation_id: u64,
}

impl CompactionEnvironment {
    pub(crate) fn policy(&self) -> &tauqe_core::config::HistoryConfig {
        &self.environment.snapshot.config.history
    }

    pub(crate) async fn scribe(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
    ) -> Result<StructuredExecution, TurnProviderError> {
        if self.operation_id != operation.id() || !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let model = self.environment.snapshot.config.history_model();
        let provider = resources
            .provider(&self.environment.snapshot.config, model.provider)
            .await?;
        if !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(StructuredExecution {
            provider,
            model,
            effort: None,
            session: None,
        })
    }
}

/// Technical configuration shared by all isolated turns of one plan run.
#[derive(Clone)]
pub(crate) struct PlanExecutionEnvironment {
    environment: Arc<ExecutionEnvironment>,
    operation_id: u64,
}

/// Operation-bound Architect requests. Rebinding preserves the executor's cache.
pub(crate) struct ArchitectResearchExecution {
    environment: PlanExecutionEnvironment,
    resources: ExecutionSession,
    pub(crate) executor: PromptExecutor,
    base_session: String,
    session: String,
    current_model: ModelRef,
}

impl ArchitectResearchExecution {
    pub(crate) async fn execute_round(
        &mut self,
        input: PromptInput,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<Option<Result<PlanRefineRound, String>>, ExecutionError> {
        self.executor
            .prepare(input)?
            .execute_decoded(events, cancel, PlanRefinePromptProtocol::decode_round)
            .await
    }

    pub(crate) fn config(&self) -> &tauqe_core::config::AppConfig {
        self.environment.config()
    }

    pub(crate) fn max_turn_rounds(&self) -> usize {
        WorkflowOptions::from_app_config(self.environment.config()).max_turn_rounds()
    }

    pub(crate) fn models(&self) -> &tauqe_core::config::ModelsConfig {
        self.environment.models()
    }

    pub(crate) async fn select_tier(
        &mut self,
        tier: &ArchitectTier,
    ) -> Result<(), TurnProviderError> {
        if self.resources.operations.current_id() != Some(self.environment.operation_id) {
            return Err(TurnProviderError::foreign_operation());
        }
        let provider = self
            .resources
            .provider(self.config(), tier.model.provider)
            .await?;
        if self.resources.operations.current_id() != Some(self.environment.operation_id) {
            return Err(TurnProviderError::foreign_operation());
        }
        if tier.model != self.current_model {
            let index = self
                .models()
                .architect_tier_index(Some(tier))
                .unwrap_or_default();
            self.session = format_session_id(&self.base_session, &format!("tier{index}"));
        }
        self.current_model = tier.model.clone();
        self.executor.rebind(
            provider,
            tier.model.name.clone(),
            tier.effort,
            Some(self.session.clone()),
        );
        Ok(())
    }
}

impl PlanExecutionEnvironment {
    pub(crate) fn compaction_environment(&self) -> CompactionEnvironment {
        CompactionEnvironment {
            environment: Arc::clone(&self.environment),
            operation_id: self.operation_id,
        }
    }

    pub(crate) fn resolve_architect_model(&self, selection: &ModelSelection) -> ModelRef {
        self.config().resolve_model(selection)
    }

    fn architect_session(scope: &ArchitectScope) -> String {
        match scope {
            ArchitectScope::Plan(id) => format_session_id(SESSION_PREFIX_ARCHITECT, id),
            ArchitectScope::Review(id) => format_session_id(SESSION_PREFIX_REVIEW, id),
        }
    }

    pub(crate) async fn for_structured(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        tier: &ArchitectTier,
        scope: &ArchitectScope,
    ) -> Result<StructuredExecution, TurnProviderError> {
        self.structured_execution(
            resources,
            operation,
            tier.model.clone(),
            tier.effort,
            Some(format_session_id(
                &Self::architect_session(scope),
                &format!(
                    "critique{}",
                    self.models()
                        .architect_tier_index(Some(tier))
                        .unwrap_or_default()
                ),
            )),
        )
        .await
    }

    /// Bind post-analysis to its review identity without a critique rung suffix.
    pub(crate) async fn for_review_post_analysis(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        tier: &ArchitectTier,
        scope: &ArchitectScope,
    ) -> Result<StructuredExecution, TurnProviderError> {
        self.structured_execution(
            resources,
            operation,
            tier.model.clone(),
            tier.effort,
            Some(Self::architect_session(scope)),
        )
        .await
    }

    /// Bind a read-only audit to the selected Reviewer model and review identity.
    pub(crate) async fn for_reviewer_structured(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        selection: &ModelSelection,
        scope: &ArchitectScope,
    ) -> Result<StructuredExecution, TurnProviderError> {
        self.structured_execution(
            resources,
            operation,
            self.config().resolve_model(selection),
            self.config()
                .resolve_selection_effort(ModelRole::Reviewer, selection),
            Some(Self::architect_session(scope)),
        )
        .await
    }

    /// Auxiliary Scribe calls have no session affinity or reasoning effort.
    pub(crate) async fn for_scribe_structured(
        &self,
        application: &Application,
        operation: &OperationGuard,
        selection: &ModelSelection,
    ) -> Result<StructuredExecution, TurnProviderError> {
        self.structured_execution(
            &application.execution_session(),
            operation,
            self.config().resolve_model(selection),
            None,
            None,
        )
        .await
    }

    async fn structured_execution(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        model: ModelRef,
        effort: Option<ReasoningEffort>,
        session: Option<String>,
    ) -> Result<StructuredExecution, TurnProviderError> {
        let provider = self
            .structured_provider(resources, operation, model.provider)
            .await?;
        Ok(StructuredExecution {
            provider,
            model,
            effort,
            session,
        })
    }

    async fn structured_provider(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        kind: ProviderKind,
    ) -> Result<Arc<dyn LlmProvider>, TurnProviderError> {
        if self.operation_id != operation.id() || !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let provider = resources.provider(self.config(), kind).await?;
        if !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(provider)
    }

    pub(crate) async fn for_architect_research(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        tier: &ArchitectTier,
        scope: &ArchitectScope,
    ) -> Result<ArchitectResearchExecution, TurnProviderError> {
        if self.operation_id != operation.id() || !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let provider = resources
            .provider(self.config(), tier.model.provider)
            .await?;
        if !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let session = Self::architect_session(scope);
        Ok(ArchitectResearchExecution {
            environment: self.clone(),
            resources: resources.clone(),
            executor: PromptExecutor::new(
                provider,
                Box::new(PlanRefinePromptProtocol::default()),
                tier.model.name.clone(),
                tier.effort,
                Some(session.clone()),
                self.config().repl.patch_strategy(),
            ),
            base_session: session.clone(),
            session,
            current_model: tier.model.clone(),
        })
    }

    pub(crate) fn models(&self) -> &tauqe_core::config::ModelsConfig {
        &self.environment.snapshot.config.models
    }
    pub(crate) fn config(&self) -> &tauqe_core::config::AppConfig {
        &self.environment.snapshot.config
    }
    pub(crate) fn resolve_step_model(
        &self,
        selection: Option<ModelSelection>,
    ) -> ResolvedTurnModel {
        self.environment
            .resolve_model(selection, None, ModelRole::Coder)
    }
    pub(crate) fn new(snapshot: ConfigurationSnapshot, operation: &OperationGuard) -> Self {
        Self {
            environment: Arc::new(ExecutionEnvironment::new(snapshot)),
            operation_id: operation.id(),
        }
    }

    pub(crate) async fn bind_step(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        session: Option<String>,
        capabilities: CapabilityTree,
    ) -> Result<BoundExecution, TurnProviderError> {
        if self.operation_id != operation.id() {
            return Err(TurnProviderError::foreign_operation());
        }
        self.environment
            .bind_step(
                resources,
                operation,
                selection,
                effort,
                session,
                capabilities,
            )
            .await
    }
}

/// Technical dependencies for a prompt, independent of disk application policy.
pub(crate) struct BoundPrompt {
    provider: Arc<dyn LlmProvider>,
    session_id: Option<String>,
    protocol: Box<dyn PromptProtocol>,
}

impl BoundPrompt {
    pub(crate) fn into_executor(
        self,
        model: &ModelRef,
        effort: Option<ReasoningEffort>,
        patch_strategy: PatchStrategy,
    ) -> PromptExecutor {
        PromptExecutor::new(
            self.provider,
            self.protocol,
            model.name.clone(),
            effort,
            self.session_id,
            patch_strategy,
        )
    }
}

/// Bind one attempt without starting a round or opening a disk transaction.
pub(crate) struct BoundExecution {
    pub(crate) model: ResolvedTurnModel,
    pub(crate) prompt: BoundPrompt,
    pub(crate) policy: Box<dyn ApplyPolicy>,
}

/// Read-only request bound to the frozen operation environment. Its contract
/// supplies the decoder, visible field filter, and completion policy together.
#[derive(Clone)]
pub(crate) struct StructuredExecution {
    provider: Arc<dyn LlmProvider>,
    model: tauqe_core::models::ModelRef,
    effort: Option<ReasoningEffort>,
    session: Option<String>,
}

impl StructuredExecution {
    pub(crate) async fn run<T>(
        &self,
        contract: &StructuredContract<T>,
        messages: Vec<ChatMessage>,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<T, StructuredError> {
        structured::execute(
            self.provider.as_ref(),
            &self.model.name,
            self.effort,
            self.session.as_deref(),
            contract,
            messages,
            events,
            cancel,
        )
        .await
    }
}

impl<'a> ExecutionFactory<'a> {
    pub(crate) fn compaction_environment(&self) -> CompactionEnvironment {
        CompactionEnvironment {
            environment: Arc::new(ExecutionEnvironment::new(self.environment.snapshot.clone())),
            operation_id: self.operation.id(),
        }
    }

    pub(crate) fn selection(&self) -> &ModelSelection {
        &self.environment.snapshot.selection
    }

    pub(crate) fn resolve_model(
        &self,
        selection: ModelSelection,
        effort_role: ModelRole,
    ) -> ResolvedTurnModel {
        self.environment
            .resolve_model(Some(selection), None, effort_role)
    }

    pub(crate) async fn bind_prompt(
        &self,
        model: &ModelRef,
        effort: Option<ReasoningEffort>,
        session_id: Option<String>,
        patch_strategy: PatchStrategy,
    ) -> Result<PromptExecutor, TurnProviderError> {
        let prompt = self
            .environment
            .bind_prompt(
                &self.application.execution_session(),
                self.operation,
                model.provider,
                session_id,
                CapabilityTree::set([]),
            )
            .await?;
        Ok(prompt.into_executor(model, effort, patch_strategy))
    }

    pub(crate) fn apply_policy(&self, workflow: WorkflowKind) -> Box<dyn ApplyPolicy> {
        self.application
            .execution_session()
            .policy(workflow, self.operation.cancel_receiver())
    }

    pub(crate) async fn bind(
        application: &'a Application,
        operation: &'a OperationGuard,
    ) -> Result<Self, TurnProviderError> {
        if !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let snapshot = application.configuration().await;
        if !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(Self {
            application,
            operation,
            environment: ExecutionEnvironment::new(snapshot),
        })
    }

    #[cfg(test)]
    async fn bind_step(
        &self,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        session_id: Option<String>,
        capabilities: CapabilityTree,
    ) -> Result<BoundExecution, TurnProviderError> {
        self.environment
            .bind_step(
                &self.application.execution_session(),
                self.operation,
                selection,
                effort,
                session_id,
                capabilities,
            )
            .await
    }
}

impl ExecutionEnvironment {
    pub(crate) fn new(snapshot: ConfigurationSnapshot) -> Self {
        Self { snapshot }
    }

    async fn bind_step(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        session_id: Option<String>,
        capabilities: CapabilityTree,
    ) -> Result<BoundExecution, TurnProviderError> {
        let model = self.resolve_model(selection, effort, ModelRole::Coder);
        let prompt = self
            .bind_prompt(
                resources,
                operation,
                model.model.provider,
                session_id,
                capabilities,
            )
            .await?;
        let policy = resources.policy(model.config.repl.workflow, operation.cancel_receiver());
        Ok(BoundExecution {
            model,
            prompt,
            policy,
        })
    }

    async fn bind_prompt(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        kind: ProviderKind,
        session_id: Option<String>,
        capabilities: CapabilityTree,
    ) -> Result<BoundPrompt, TurnProviderError> {
        if !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let provider = resources.provider(&self.snapshot.config, kind).await?;
        if !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(BoundPrompt {
            provider,
            session_id,
            protocol: Box::new(JsonPromptProtocol::new(capabilities)),
        })
    }

    pub(crate) fn resolve_model(
        &self,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        effort_role: ModelRole,
    ) -> ResolvedTurnModel {
        let config = self.snapshot.config.clone();
        let selection = selection.unwrap_or_else(|| self.snapshot.selection.clone());
        let model = config.resolve_model(&selection);
        let role = match &selection {
            ModelSelection::Role(role) => Some(*role),
            ModelSelection::Specific(_) | ModelSelection::Tier { .. } => None,
        };
        let effort = effort.or_else(|| config.resolve_selection_effort(effort_role, &selection));
        let alias = config.model_alias(&model).map(str::to_string);
        ResolvedTurnModel {
            config,
            selection,
            model,
            role,
            effort,
            alias,
        }
    }

    #[cfg(test)]
    pub(crate) async fn provider(
        &self,
        application: &Application,
        kind: ProviderKind,
    ) -> Result<Arc<dyn LlmProvider>, TurnProviderError> {
        application
            .execution_session()
            .provider(&self.snapshot.config, kind)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::configuration::ReloadSelection;
    use tauqe_core::config::{AppConfig, ModelsConfig, OpenRouterConfig, ProvidersConfig};
    use tauqe_core::execution::{PromptInput, PromptSequence};
    use tauqe_core::llm::step::turn_capability_expr;
    use tauqe_core::models::{ModelEntry, ModelRef};
    use tauqe_core::prompt::TurnBrief;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};

    fn config(key: Option<&str>) -> AppConfig {
        AppConfig {
            models: ModelsConfig {
                repl: vec![ModelEntry::new(
                    ModelRef::openrouter("base/model"),
                    Some(ReasoningEffort::Low),
                )],
                coder: Some(vec![ModelEntry::new(
                    ModelRef::openrouter("base/model"),
                    Some(ReasoningEffort::High),
                )]),
                ..Default::default()
            },
            providers: ProvidersConfig {
                openrouter: Some(OpenRouterConfig {
                    api_key: key.map(str::to_owned),
                    models: vec![],
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn prompt_binding_uses_frozen_credentials_after_reload() {
        let app = Application::new(config(Some("test-key")));
        let operation = app.try_begin_operation().unwrap();
        let factory = ExecutionFactory::bind(&app, &operation).await.unwrap();
        let model = factory.resolve_model(factory.selection().clone(), ModelRole::REPL);
        app.replace_configuration(config(None), ReloadSelection::Default)
            .await;
        assert!(factory
            .bind_prompt(
                &model.model,
                model.effort,
                Some("frozen-session".into()),
                model.config.repl.patch_strategy()
            )
            .await
            .is_ok());
        assert_eq!(model.effort, Some(ReasoningEffort::Low));
        assert!(ExecutionFactory::bind(&app, &operation)
            .await
            .unwrap()
            .bind_step(None, None, None, turn_capability_expr())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn foreign_and_stale_operations_cannot_resolve() {
        let app = Application::new(config(None));
        let provider = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        assert!(ExecutionFactory::bind(&app, &foreign).await.is_err());
        let guard = app.try_begin_operation().unwrap();
        let factory = ExecutionFactory::bind(&app, &guard).await.unwrap();
        assert!(app.release_operation_registration(guard.id()));
        let replacement = app.try_begin_operation().unwrap();
        assert!(factory
            .bind_step(None, None, None, turn_capability_expr())
            .await
            .is_err());
        assert!(provider.requests().is_empty());
        assert_eq!(app.active_operation_id(), Some(replacement.id()));
    }

    #[tokio::test]
    async fn next_rung_switches_provider_kind_without_reusing_the_previous_client() {
        let app = Application::new(config(None));
        let router = Arc::new(MockLlmProvider::new());
        let openai = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(ProviderKind::OpenRouter, router.clone())
            .await;
        app.test_set_provider_for(ProviderKind::OpenAI, openai.clone())
            .await;
        let guard = app.try_begin_operation().unwrap();
        let factory = ExecutionFactory::bind(&app, &guard).await.unwrap();
        let first = factory
            .bind_step(
                Some(ModelSelection::Role(ModelRole::Coder)),
                None,
                None,
                turn_capability_expr(),
            )
            .await
            .unwrap();
        let second = factory
            .bind_step(
                Some(ModelSelection::Tier {
                    model: ModelRef {
                        provider: ProviderKind::OpenAI,
                        name: "next/model".into(),
                    },
                    effort: Some(ReasoningEffort::Max),
                }),
                None,
                Some("step-affinity".into()),
                turn_capability_expr(),
            )
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            &first.prompt.provider,
            &(router as Arc<dyn LlmProvider>)
        ));
        assert!(Arc::ptr_eq(
            &second.prompt.provider,
            &(openai as Arc<dyn LlmProvider>)
        ));
        assert_eq!(second.model.effort, Some(ReasoningEffort::Max));
        assert_eq!(second.prompt.session_id.as_deref(), Some("step-affinity"));
    }

    #[tokio::test]
    async fn plan_environment_keeps_configuration_across_steps_and_provider_changes() {
        let mut initial = config(Some("test-key"));
        initial.repl = toml::from_str::<AppConfig>(
            "[repl]\nmax_turn_rounds = 7\npatch_strategy = 'search_replace'\n",
        )
        .unwrap()
        .repl;
        let app = Application::new(initial);
        let refusal = || {
            MockResponse::text(
                r#"{"message":"Cannot continue","action":{"kind":"blocked","blocked":{"reason":"Missing prerequisite","details":"","is_blocked":true}}}"#,
            )
        };
        let router = Arc::new(MockLlmProvider::scripted([refusal()]));
        let openai = Arc::new(MockLlmProvider::scripted([refusal()]));
        app.test_set_provider_for(ProviderKind::OpenRouter, router.clone())
            .await;
        app.test_set_provider_for(ProviderKind::OpenAI, openai.clone())
            .await;
        let guard = app.try_begin_operation().unwrap();
        let frozen = PlanExecutionEnvironment::new(app.configuration().await, &guard);
        let mut changed = config(None);
        changed.repl = toml::from_str::<AppConfig>("[repl]\nmax_turn_rounds = 1\n")
            .unwrap()
            .repl;
        app.replace_configuration(changed, ReloadSelection::Default)
            .await;
        let capabilities = turn_capability_expr();
        let first = frozen
            .bind_step(
                &app.execution_session(),
                &guard,
                Some(ModelSelection::Role(ModelRole::Coder)),
                None,
                Some("plan-affinity".into()),
                capabilities.clone(),
            )
            .await
            .unwrap();
        let second = frozen
            .bind_step(
                &app.execution_session(),
                &guard,
                Some(ModelSelection::Tier {
                    model: ModelRef {
                        provider: ProviderKind::OpenAI,
                        name: "next/model".into(),
                    },
                    effort: Some(ReasoningEffort::Max),
                }),
                None,
                Some("plan-affinity".into()),
                capabilities,
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&first.model.config.repl).unwrap()["max_turn_rounds"],
            7
        );
        assert_eq!(
            serde_json::to_value(&second.model.config.repl).unwrap()["max_turn_rounds"],
            7
        );
        assert_eq!(
            first
                .model
                .config
                .providers
                .openrouter
                .as_ref()
                .unwrap()
                .api_key
                .as_deref(),
            Some("test-key")
        );
        assert_eq!(first.model.effort, Some(ReasoningEffort::High));
        assert_eq!(second.model.effort, Some(ReasoningEffort::Max));
        for (bound, provider, name, effort) in [
            (first, &router, "base/model", ReasoningEffort::High),
            (second, &openai, "next/model", ReasoningEffort::Max),
        ] {
            let BoundExecution { model, prompt, .. } = bound;
            let mut executor = prompt.into_executor(
                &model.model,
                model.effort,
                model.config.repl.patch_strategy(),
            );
            assert_eq!(executor.patch_strategy(), PatchStrategy::SearchReplace);
            executor
                .prepare(PromptInput {
                    task: "Execute the isolated step".into(),
                    originals: vec![],
                    current_files: vec![],
                    history: None,
                    capabilities: turn_capability_expr(),
                    project: Default::default(),
                    repo_root: Default::default(),
                    repo_map: None,
                    repo_dirs: None,
                    outer_docs: vec![],
                    brief: Some(TurnBrief::step()),
                    sequence: PromptSequence::Assemble { continuation: None },
                    tail: vec![],
                    excluded_disk_sources: vec![],
                })
                .unwrap()
                .execute(&EventSink::noop(), &CancellationToken::new())
                .await
                .unwrap();
            let request = provider.last_request().unwrap();
            assert_eq!(request.model, name);
            assert_eq!(request.reasoning_effort, Some(effort));
            assert_eq!(request.session_id.as_deref(), Some("plan-affinity"));
            let schema = serde_json::to_value(request.response_format).unwrap();
            let branches = schema
                .pointer("/json_schema/schema/properties/action/anyOf")
                .unwrap()
                .as_array()
                .unwrap();
            let kinds: Vec<_> = branches
                .iter()
                .map(|branch| branch["properties"]["kind"]["enum"][0].as_str().unwrap())
                .collect();
            assert_eq!(kinds, ["research", "edit", "blocked", "abort"]);
            assert!(branches[0]["properties"].get("doc_requests").is_none());
            assert_eq!(branches[1]["properties"]["changes"]["type"], "array");
            assert!(branches[1]["properties"].get("patch").is_none());
        }

        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        assert!(frozen
            .bind_step(
                &other.execution_session(),
                &foreign,
                None,
                None,
                None,
                turn_capability_expr()
            )
            .await
            .is_err());
        assert!(app.release_operation_registration(guard.id()));
        let replacement = app.try_begin_operation().unwrap();
        assert!(frozen
            .bind_step(
                &app.execution_session(),
                &replacement,
                None,
                None,
                None,
                turn_capability_expr()
            )
            .await
            .is_err());
        assert_eq!(router.requests().len(), 1);
        assert_eq!(openai.requests().len(), 1);
        assert!(frozen
            .for_structured(
                &app.execution_session(),
                &replacement,
                &ArchitectTier::from(ModelRef::openrouter("base/model")),
                &ArchitectScope::Plan("plan".into()),
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn structured_execution_uses_the_plan_environment_after_reload() {
        let app = Application::new(config(Some("frozen-key")));
        let router = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(ProviderKind::OpenRouter, router.clone())
            .await;
        let guard = app.try_begin_operation().unwrap();
        let frozen = PlanExecutionEnvironment::new(app.configuration().await, &guard);
        app.replace_configuration(config(None), ReloadSelection::Default)
            .await;

        let mut tier = ArchitectTier::from(ModelRef::openrouter("base/model"));
        tier.effort = Some(ReasoningEffort::High);
        let execution = frozen
            .for_structured(
                &app.execution_session(),
                &guard,
                &tier,
                &ArchitectScope::Plan("plan".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            frozen
                .config()
                .providers
                .openrouter
                .as_ref()
                .unwrap()
                .api_key
                .as_deref(),
            Some("frozen-key")
        );
        assert!(Arc::ptr_eq(
            &execution.provider,
            &(router as Arc<dyn LlmProvider>)
        ));
        assert_eq!(execution.model, tier.model);
        assert_eq!(execution.effort, tier.effort);
        assert_eq!(
            execution.session.as_deref(),
            Some("tauqe-architect-plan-critique0")
        );
    }

    #[tokio::test]
    async fn reviewer_selection_preserves_the_exact_tier_and_review_affinity() {
        let shared = ModelRef::openrouter("shared/reviewer");
        let mut initial = config(None);
        initial.models.reviewer = Some(vec![
            ModelEntry::new(shared.clone(), Some(ReasoningEffort::Low)),
            ModelEntry::new(shared.clone(), Some(ReasoningEffort::High)),
        ]);
        let app = Application::new(initial);
        let provider = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let guard = app.try_begin_operation().unwrap();
        let frozen = PlanExecutionEnvironment::new(app.configuration().await, &guard);
        app.replace_configuration(config(None), ReloadSelection::Default)
            .await;

        let scope = ArchitectScope::Review("audit".into());
        let role = frozen
            .for_reviewer_structured(
                &app.execution_session(),
                &guard,
                &ModelSelection::Role(ModelRole::Reviewer),
                &scope,
            )
            .await
            .unwrap();
        assert_eq!(role.model, shared);
        assert_eq!(role.effort, Some(ReasoningEffort::Low));
        assert_eq!(role.session.as_deref(), Some("tauqe-review-audit"));

        let tier = frozen
            .for_reviewer_structured(
                &app.execution_session(),
                &guard,
                &ModelSelection::Tier {
                    model: shared.clone(),
                    effort: Some(ReasoningEffort::High),
                },
                &scope,
            )
            .await
            .unwrap();
        assert_eq!(tier.model, shared);
        assert_eq!(tier.effort, Some(ReasoningEffort::High));
        assert_eq!(tier.session.as_deref(), Some("tauqe-review-audit"));
        assert!(Arc::ptr_eq(
            &tier.provider,
            &(provider as Arc<dyn LlmProvider>)
        ));
    }

    #[tokio::test]
    async fn scribe_has_no_effort_or_affinity_and_stale_operations_are_refused() {
        let mut initial = config(None);
        initial.models.scribe = Some(ModelRef::openrouter("scribe/model"));
        let app = Application::new(initial);
        let provider = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let guard = app.try_begin_operation().unwrap();
        let frozen = PlanExecutionEnvironment::new(app.configuration().await, &guard);
        let selection = ModelSelection::Role(ModelRole::Scribe);
        let execution = frozen
            .for_scribe_structured(&app, &guard, &selection)
            .await
            .unwrap();
        assert_eq!(execution.model, ModelRef::openrouter("scribe/model"));
        assert_eq!(execution.effort, None);
        assert_eq!(execution.session, None);
        assert!(Arc::ptr_eq(
            &execution.provider,
            &(provider as Arc<dyn LlmProvider>)
        ));

        assert!(app.release_operation_registration(guard.id()));
        let replacement = app.try_begin_operation().unwrap();
        assert!(frozen
            .for_scribe_structured(&app, &guard, &selection)
            .await
            .is_err());
        assert!(frozen
            .for_reviewer_structured(
                &app.execution_session(),
                &replacement,
                &ModelSelection::Role(ModelRole::Reviewer),
                &ArchitectScope::Review("audit".into()),
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn architect_affinity_rotates_only_when_the_model_changes() {
        let app = Application::new(config(None));
        let router = Arc::new(MockLlmProvider::new());
        let openai = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(ProviderKind::OpenRouter, router)
            .await;
        app.test_set_provider_for(ProviderKind::OpenAI, openai)
            .await;
        let guard = app.try_begin_operation().unwrap();
        let frozen = PlanExecutionEnvironment::new(app.configuration().await, &guard);
        let scope = ArchitectScope::Review("audit".into());
        let first = ArchitectTier::from(ModelRef::openrouter("base/model"));
        let mut execution = frozen
            .for_architect_research(&app.execution_session(), &guard, &first, &scope)
            .await
            .unwrap();
        assert_eq!(execution.session, "tauqe-review-audit");
        let mut deeper = first.clone();
        deeper.effort = Some(ReasoningEffort::High);
        execution.select_tier(&deeper).await.unwrap();
        assert_eq!(execution.session, "tauqe-review-audit");
        let switched = ArchitectTier::from(ModelRef {
            provider: ProviderKind::OpenAI,
            name: "next/model".into(),
        });
        execution.select_tier(&switched).await.unwrap();
        assert_eq!(execution.session, "tauqe-review-audit-tier0");
        assert_eq!(execution.current_model, switched.model);
    }
}
