//! Technical execution dependencies frozen for one application operation.

use crate::api::{Application, OperationGuard};
use crate::application::configuration::ConfigurationSnapshot;
use crate::application::plan_horizon::ArchitectScope;
use crate::application::turn_setup::{ResolvedTurnModel, TurnMode, TurnProviderError};
use std::{path::Path, sync::Arc};
use tauqe_core::context::ContextManager;
use tauqe_core::control::context::{CancellationToken, EventSink};
use tauqe_core::control::execution::{
    BoundedUnitRequest, ContinuationRequest, ExecutionUnit, TurnExecutionConfig,
};
use tauqe_core::control::outcome::ResponseEnvelope;
use tauqe_core::control::outcome::{LoopResult, SystemError};
use tauqe_core::control::research_unit::{
    StructuredResearchError, StructuredResearchProgress, StructuredResearchReply,
    StructuredResearchUnit,
};
use tauqe_core::control::structured::{self, StructuredContract, StructuredError};
use tauqe_core::control::turn::{TurnArtifact, TurnCompletion, TurnLoopConfig};
use tauqe_core::history::HistorySnapshot;
use tauqe_core::llm::CapabilityTree;
use tauqe_core::llm::LlmCommand;
use tauqe_core::models::{
    ArchitectTier, ModelRef, ModelRole, ModelSelection, ProviderKind, ReasoningEffort,
};
use tauqe_core::prompt::refine::PlanRefinePromptProtocol;
use tauqe_core::prompt::{json::JsonPromptProtocol, protocol::PromptProtocol, TurnBrief};
use tauqe_core::providers::{
    format_session_id, ChatMessage, LlmProvider, SESSION_PREFIX_ARCHITECT, SESSION_PREFIX_REPL,
    SESSION_PREFIX_REVIEW,
};
use tauqe_core::workflow::policy::{ApplyPolicy, DirectApplyPolicy};
use tauqe_core::workflow::WorkflowKind;
use tauqe_core::workflow::WorkflowOptions;
use tokio::sync::watch;

/// Frozen environment of an admitted operation. The operation is checked again
/// at resolution time so a stale guard cannot start work under its successor.
pub(crate) struct ExecutionFactory<'a> {
    application: &'a Application,
    operation: &'a OperationGuard,
    environment: ExecutionEnvironment,
    dialogue_id: String,
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
        application: &Application,
        operation: &OperationGuard,
    ) -> Result<StructuredExecution, TurnProviderError> {
        if self.operation_id != operation.id() || !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let model = self.environment.snapshot.config.history_model();
        let provider = self
            .environment
            .provider(application, model.provider)
            .await?;
        if !application.owns_operation(operation) {
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

/// One Architect research unit; changing the rung retains its journal and cache.
pub(crate) struct ArchitectResearchExecution {
    environment: PlanExecutionEnvironment,
    application: Application,
    operation_id: u64,
    provider: Arc<dyn LlmProvider>,
    unit: StructuredResearchUnit,
    base_session: String,
    session: String,
    current_model: ModelRef,
}

impl ArchitectResearchExecution {
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
        if self.application.active_operation_id() != Some(self.operation_id) {
            return Err(TurnProviderError::foreign_operation());
        }
        self.provider = self
            .environment
            .environment
            .provider(&self.application, tier.model.provider)
            .await?;
        if self.application.active_operation_id() != Some(self.operation_id) {
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
        Ok(())
    }

    pub(crate) fn feedback(&mut self, text: &str) {
        self.unit.feedback(text);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn request<T>(
        &mut self,
        root: &Path,
        context: &mut ContextManager,
        task: &str,
        tier: &ArchitectTier,
        grant: CapabilityTree,
        feedback: Option<&str>,
        decode: fn(&str) -> Result<T, String>,
        cancel: &CancellationToken,
        events: &EventSink,
    ) -> Result<StructuredResearchReply<T>, StructuredResearchError> {
        let protocol = PlanRefinePromptProtocol::default();
        let options = WorkflowOptions::from_app_config(self.environment.config());
        self.unit
            .request(
                TurnExecutionConfig {
                    repo_root: root,
                    provider: self.provider.as_ref(),
                    model: &tier.model,
                    protocol: &protocol,
                    config: &self.environment.environment.snapshot.config,
                    session_id: Some(self.session.clone()),
                    reasoning_effort: tier.effort,
                },
                context,
                &options,
                task,
                grant,
                feedback,
                decode,
                cancel,
                events,
            )
            .await
    }

    pub(crate) async fn research(
        &mut self,
        context: &mut ContextManager,
        root: &Path,
        commands: &[LlmCommand],
        cancel: &CancellationToken,
    ) -> StructuredResearchProgress {
        let options = WorkflowOptions::from_app_config(self.environment.config());
        self.unit
            .research(context, root, commands, &options, cancel)
            .await
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
        application: &Application,
        operation: &OperationGuard,
        tier: &ArchitectTier,
        scope: &ArchitectScope,
    ) -> Result<StructuredExecution, TurnProviderError> {
        self.structured_execution(
            application,
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
        application: &Application,
        operation: &OperationGuard,
        tier: &ArchitectTier,
        scope: &ArchitectScope,
    ) -> Result<StructuredExecution, TurnProviderError> {
        self.structured_execution(
            application,
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
        application: &Application,
        operation: &OperationGuard,
        selection: &ModelSelection,
        scope: &ArchitectScope,
    ) -> Result<StructuredExecution, TurnProviderError> {
        self.structured_execution(
            application,
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
            application,
            operation,
            self.config().resolve_model(selection),
            None,
            None,
        )
        .await
    }

    async fn structured_execution(
        &self,
        application: &Application,
        operation: &OperationGuard,
        model: ModelRef,
        effort: Option<ReasoningEffort>,
        session: Option<String>,
    ) -> Result<StructuredExecution, TurnProviderError> {
        let provider = self
            .structured_provider(application, operation, model.provider)
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
        application: &Application,
        operation: &OperationGuard,
        kind: ProviderKind,
    ) -> Result<Arc<dyn LlmProvider>, TurnProviderError> {
        if self.operation_id != operation.id() || !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let provider = self.environment.provider(application, kind).await?;
        if self.operation_id != operation.id() || !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(provider)
    }

    pub(crate) async fn for_architect_research(
        &self,
        application: &Application,
        operation: &OperationGuard,
        tier: &ArchitectTier,
        scope: &ArchitectScope,
    ) -> Result<ArchitectResearchExecution, TurnProviderError> {
        let provider = self
            .structured_provider(application, operation, tier.model.provider)
            .await?;
        Ok(ArchitectResearchExecution {
            environment: self.clone(),
            application: application.clone(),
            operation_id: operation.id(),
            provider,
            unit: StructuredResearchUnit::new(),
            base_session: Self::architect_session(scope),
            session: Self::architect_session(scope),
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
            .resolve_model(selection, None, TurnMode::Step)
    }
    pub(crate) fn new(snapshot: ConfigurationSnapshot, operation: &OperationGuard) -> Self {
        Self {
            environment: Arc::new(ExecutionEnvironment::new(snapshot)),
            operation_id: operation.id(),
        }
    }

    pub(crate) async fn for_edit_unit(
        &self,
        application: &Application,
        operation: &OperationGuard,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        session: Option<String>,
        capabilities: CapabilityTree,
    ) -> Result<EditExecution, TurnProviderError> {
        if self.operation_id != operation.id() || !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let model = self
            .environment
            .resolve_model(selection, effort, TurnMode::Step);
        let provider = self
            .environment
            .provider(application, model.model.provider)
            .await?;
        if self.operation_id != operation.id() || !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let policy = ExecutionFactory::policy(
            model.config.repl.workflow,
            operation.cancel_receiver(),
            application,
        );
        let protocol: Box<dyn PromptProtocol> = Box::new(JsonPromptProtocol::new(capabilities));
        Ok(EditExecution {
            model,
            provider,
            session_id: session,
            policy,
            protocol,
            unit: ExecutionUnit::default(),
        })
    }
}

/// Technical dependencies stay private to infrastructure; workflow callers
/// receive the resolved model metadata, not a provider client.
pub(crate) struct BoundExecution {
    pub(crate) model: ResolvedTurnModel,
    provider: Arc<dyn LlmProvider>,
    session_id: Option<String>,
    policy: Box<dyn ApplyPolicy>,
}

/// One isolated edit-capable unit with technical dependencies bound to its operation.
pub(crate) struct EditExecution {
    pub(crate) model: ResolvedTurnModel,
    provider: Arc<dyn LlmProvider>,
    session_id: Option<String>,
    policy: Box<dyn ApplyPolicy>,
    protocol: Box<dyn PromptProtocol>,
    unit: ExecutionUnit,
}

/// Technical collaborators for a dialogue, frozen under its operation lease.
pub(crate) struct DialogueExecution {
    pub(crate) model: ResolvedTurnModel,
    provider: Arc<dyn LlmProvider>,
    session_id: Option<String>,
    policy: Box<dyn ApplyPolicy>,
    unit: ExecutionUnit,
}

impl DialogueExecution {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute(
        &mut self,
        root: &Path,
        context: &mut ContextManager,
        cancel: &CancellationToken,
        sink: &EventSink,
        task: String,
        capabilities: CapabilityTree,
        history: Option<&HistorySnapshot>,
    ) -> Result<LoopResult<TurnArtifact>, SystemError> {
        let protocol = JsonPromptProtocol::new(capabilities.clone());
        self.unit
            .execute_bounded(
                TurnExecutionConfig {
                    repo_root: root,
                    provider: self.provider.as_ref(),
                    model: &self.model.model,
                    protocol: &protocol,
                    config: &self.model.config,
                    session_id: self.session_id.clone(),
                    reasoning_effort: self.model.effort,
                },
                BoundedUnitRequest {
                    context,
                    history,
                    capabilities,
                    task,
                    completion: TurnCompletion::AnswerAllowed,
                    brief: None,
                },
                &TurnLoopConfig::from_app_config(&self.model.config),
                self.policy.as_mut(),
                cancel,
                sink,
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn continue_round(
        &mut self,
        root: &Path,
        context: &mut ContextManager,
        cancel: &CancellationToken,
        sink: &EventSink,
        capabilities: CapabilityTree,
        history: Option<&HistorySnapshot>,
        diagnostics: &str,
    ) -> Result<ResponseEnvelope, SystemError> {
        let protocol = JsonPromptProtocol::new(capabilities.clone());
        self.unit
            .continue_round(
                TurnExecutionConfig {
                    repo_root: root,
                    provider: self.provider.as_ref(),
                    model: &self.model.model,
                    protocol: &protocol,
                    config: &self.model.config,
                    session_id: self.session_id.clone(),
                    reasoning_effort: self.model.effort,
                },
                ContinuationRequest {
                    context,
                    history,
                    capabilities,
                    diagnostics,
                },
                self.policy.as_mut(),
                cancel,
                sink,
            )
            .await
    }
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

impl EditExecution {
    pub(crate) async fn run(
        &mut self,
        root: &Path,
        context: &mut ContextManager,
        history: Option<&HistorySnapshot>,
        cancel: &CancellationToken,
        sink: &EventSink,
        task: String,
    ) -> Result<LoopResult<TurnArtifact>, SystemError> {
        let capabilities = self.protocol.capability_expr();
        self.unit
            .execute_bounded(
                TurnExecutionConfig {
                    repo_root: root,
                    provider: self.provider.as_ref(),
                    model: &self.model.model,
                    protocol: self.protocol.as_ref(),
                    config: &self.model.config,
                    session_id: self.session_id.clone(),
                    reasoning_effort: self.model.effort,
                },
                BoundedUnitRequest {
                    context,
                    history,
                    capabilities,
                    task,
                    completion: TurnCompletion::ActionRequired,
                    brief: Some(TurnBrief::step()),
                },
                &TurnLoopConfig::from_app_config(&self.model.config),
                self.policy.as_mut(),
                cancel,
                sink,
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

    pub(crate) async fn for_dialogue(&self) -> Result<DialogueExecution, TurnProviderError> {
        let BoundExecution {
            model,
            provider,
            session_id,
            policy,
        } = self
            .resolve(None, None, TurnMode::Interactive, None)
            .await?;
        Ok(DialogueExecution {
            model,
            provider,
            session_id,
            policy,
            unit: ExecutionUnit::default(),
        })
    }

    pub(crate) async fn for_edit_unit(
        &self,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        session: Option<String>,
        capabilities: CapabilityTree,
    ) -> Result<EditExecution, TurnProviderError> {
        let BoundExecution {
            model,
            provider,
            session_id,
            policy,
        } = self
            .resolve(selection, effort, TurnMode::Step, session)
            .await?;
        let protocol: Box<dyn PromptProtocol> =
            Box::new(JsonPromptProtocol::new(capabilities.clone()));
        debug_assert_eq!(protocol.capability_expr(), capabilities);
        Ok(EditExecution {
            model,
            provider,
            session_id,
            policy,
            protocol,
            unit: ExecutionUnit::default(),
        })
    }

    pub(crate) async fn bind(
        application: &'a Application,
        operation: &'a OperationGuard,
    ) -> Result<Self, TurnProviderError> {
        if !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let snapshot = application.configuration().await;
        let dialogue_id = application.session_uuid().await;
        if !application.owns_operation(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(Self {
            application,
            operation,
            environment: ExecutionEnvironment::new(snapshot),
            dialogue_id,
        })
    }

    pub(crate) async fn resolve(
        &self,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        mode: TurnMode,
        explicit_session: Option<String>,
    ) -> Result<BoundExecution, TurnProviderError> {
        if !self.application.owns_operation(self.operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let model = self.environment.resolve_model(selection, effort, mode);
        let provider = self
            .environment
            .provider(self.application, model.model.provider)
            .await?;
        if !self.application.owns_operation(self.operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let session_id = explicit_session.or_else(|| match mode {
            TurnMode::Interactive => {
                Some(format_session_id(SESSION_PREFIX_REPL, &self.dialogue_id))
            }
            TurnMode::Step => None,
        });
        let policy = Self::policy(
            model.config.repl.workflow,
            self.operation.cancel_receiver(),
            self.application,
        );
        Ok(BoundExecution {
            model,
            provider,
            session_id,
            policy,
        })
    }

    fn policy(
        workflow: WorkflowKind,
        cancel: watch::Receiver<bool>,
        application: &Application,
    ) -> Box<dyn ApplyPolicy> {
        match workflow {
            WorkflowKind::Naive => Box::new(DirectApplyPolicy::new()),
            WorkflowKind::Git => Box::new(application.git_transaction_policy(cancel)),
        }
    }
}

impl ExecutionEnvironment {
    pub(crate) fn new(snapshot: ConfigurationSnapshot) -> Self {
        Self { snapshot }
    }

    pub(crate) fn resolve_model(
        &self,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        mode: TurnMode,
    ) -> ResolvedTurnModel {
        let config = self.snapshot.config.clone();
        let selection = selection.unwrap_or_else(|| self.snapshot.selection.clone());
        let model = config.resolve_model(&selection);
        let role = match &selection {
            ModelSelection::Role(role) => Some(*role),
            ModelSelection::Specific(_) | ModelSelection::Tier { .. } => None,
        };
        let effort_role = match mode {
            TurnMode::Interactive => role.unwrap_or(ModelRole::REPL),
            TurnMode::Step => ModelRole::Coder,
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

    pub(crate) async fn provider(
        &self,
        application: &Application,
        kind: ProviderKind,
    ) -> Result<Arc<dyn LlmProvider>, TurnProviderError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(provider) = application.scripted_provider(kind).await {
            return Ok(provider);
        }
        tauqe_core::providers::create_provider_with_login(
            kind,
            &self.snapshot.config,
            application.chatgpt_login.clone(),
        )
        .map(Arc::from)
        .map_err(|error| TurnProviderError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::configuration::ReloadSelection;
    use tauqe_core::config::{AppConfig, ModelsConfig, OpenRouterConfig, ProvidersConfig};
    use tauqe_core::models::{ModelEntry, ModelRef};
    use tauqe_core::providers::mock::MockLlmProvider;

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
    async fn frozen_credentials_and_affinity_survive_reload() {
        let app = Application::new(config(Some("test-key")));
        let operation = app.try_begin_operation().unwrap();
        let factory = ExecutionFactory::bind(&app, &operation).await.unwrap();
        let identity = app.session_uuid().await;
        app.replace_configuration(config(None), ReloadSelection::Default)
            .await;
        app.reset_session_uuid().await;
        let bound = factory
            .resolve(None, None, TurnMode::Interactive, None)
            .await
            .unwrap();
        assert_eq!(
            bound.session_id.as_deref(),
            Some(format!("tauqe-repl-{identity}").as_str())
        );
        assert_eq!(bound.model.effort, Some(ReasoningEffort::Low));
        assert!(ExecutionFactory::bind(&app, &operation)
            .await
            .unwrap()
            .resolve(None, None, TurnMode::Step, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn foreign_and_stale_operations_cannot_resolve() {
        let app = Application::new(config(None));
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        assert!(ExecutionFactory::bind(&app, &foreign).await.is_err());
        let guard = app.try_begin_operation().unwrap();
        let factory = ExecutionFactory::bind(&app, &guard).await.unwrap();
        assert!(app.release_operation_registration(guard.id()));
        let replacement = app.try_begin_operation().unwrap();
        assert!(factory
            .resolve(None, None, TurnMode::Step, None)
            .await
            .is_err());
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
            .resolve(
                Some(ModelSelection::Role(ModelRole::Coder)),
                None,
                TurnMode::Step,
                None,
            )
            .await
            .unwrap();
        let second = factory
            .resolve(
                Some(ModelSelection::Tier {
                    model: ModelRef {
                        provider: ProviderKind::OpenAI,
                        name: "next/model".into(),
                    },
                    effort: Some(ReasoningEffort::Max),
                }),
                None,
                TurnMode::Step,
                Some("step-affinity".into()),
            )
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            &first.provider,
            &(router as Arc<dyn LlmProvider>)
        ));
        assert!(Arc::ptr_eq(
            &second.provider,
            &(openai as Arc<dyn LlmProvider>)
        ));
        assert_eq!(second.model.effort, Some(ReasoningEffort::Max));
        assert_eq!(second.session_id.as_deref(), Some("step-affinity"));
    }

    #[tokio::test]
    async fn plan_environment_keeps_configuration_across_steps_and_provider_changes() {
        let mut initial = config(Some("test-key"));
        initial.repl = toml::from_str::<AppConfig>("[repl]\nmax_turn_rounds = 7\n")
            .unwrap()
            .repl;
        let app = Application::new(initial);
        let router = Arc::new(MockLlmProvider::new());
        let openai = Arc::new(MockLlmProvider::new());
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
        let capabilities = tauqe_core::control::turn::turn_capability_expr();
        let first = frozen
            .for_edit_unit(
                &app,
                &guard,
                Some(ModelSelection::Role(ModelRole::Coder)),
                None,
                Some("plan-affinity".into()),
                capabilities.clone(),
            )
            .await
            .unwrap();
        let second = frozen
            .for_edit_unit(
                &app,
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
        assert_eq!(first.session_id.as_deref(), Some("plan-affinity"));
        assert_eq!(second.session_id.as_deref(), Some("plan-affinity"));
        assert!(Arc::ptr_eq(
            &first.provider,
            &(router as Arc<dyn LlmProvider>)
        ));
        assert!(Arc::ptr_eq(
            &second.provider,
            &(openai as Arc<dyn LlmProvider>)
        ));

        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        assert!(frozen
            .for_edit_unit(
                &other,
                &foreign,
                None,
                None,
                None,
                tauqe_core::control::turn::turn_capability_expr()
            )
            .await
            .is_err());
        assert!(app.release_operation_registration(guard.id()));
        let replacement = app.try_begin_operation().unwrap();
        assert!(frozen
            .for_edit_unit(
                &app,
                &replacement,
                None,
                None,
                None,
                tauqe_core::control::turn::turn_capability_expr()
            )
            .await
            .is_err());
        assert!(frozen
            .for_structured(
                &app,
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
            .for_structured(&app, &guard, &tier, &ArchitectScope::Plan("plan".into()))
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
                &app,
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
                &app,
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
                &app,
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
            .for_architect_research(&app, &guard, &first, &scope)
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
