//! Technical execution dependencies frozen for one application operation.

use crate::api::{Application, OperationGuard};
use crate::application::configuration::ConfigurationSnapshot;
use crate::application::execution_scope::ExecutionScope;
use crate::application::operation::OperationRegistry;
use crate::application::turn_setup::{ResolvedTurnModel, TurnProviderError};
use crate::application::usage::UsageSession;
use crate::workflows::architect::ArchitectScope;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::execution::executor::PromptIdentity;
use tauqe_core::execution::structured::{self, StructuredContract, StructuredError};
use tauqe_core::execution::usage::UsageBinding;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptExecutor, ReadyPromptExecutor,
};
use tauqe_core::llm::CapabilityTree;
use tauqe_core::models::{
    ArchitectTier, ModelRef, ModelRole, ModelSelection, ProviderKind, ReasoningEffort,
};
use tauqe_core::prompt::refine::PlanRefinePromptProtocol;
use tauqe_core::prompt::{json::JsonPromptProtocol, protocol::PromptProtocol};
use tauqe_core::providers::{
    format_session_id, ChatMessage, LlmProvider, SESSION_PREFIX_ARCHITECT, SESSION_PREFIX_REPL,
    SESSION_PREFIX_REVIEW,
};
use tauqe_core::stop::HardCancellation;
use tauqe_core::workflow::policy::{ApplyPolicy, DirectApplyPolicy};
use tauqe_core::workflow::WorkflowKind;
use tauqe_core::workflow::{PatchStrategy, WorkflowOptions};

#[cfg(any(test, feature = "test-support"))]
pub(crate) type ProviderOverrides =
    Arc<tokio::sync::Mutex<std::collections::HashMap<ProviderKind, Arc<dyn LlmProvider>>>>;

/// Session resources needed to bind technical execution without the facade.
#[derive(Clone)]
pub(crate) struct ExecutionSession {
    pub operations: Arc<OperationRegistry>,
    pub transaction: Arc<std::sync::atomic::AtomicBool>,
    pub login: Option<Arc<tauqe_core::providers::ChatGptSignIn>>,
    pub usage: UsageSession,
    #[cfg(any(test, feature = "test-support"))]
    pub provider_override: ProviderOverrides,
}

impl ExecutionSession {
    /// Prepare accounting without opening a window; the scenario owns its boundary.
    #[allow(dead_code)] // Routes adopt this when their accounting is migrated.
    pub(crate) fn prepare_scope(&self) -> ExecutionScope {
        ExecutionScope::prepare(&self.usage)
    }

    pub(crate) fn owns(&self, operation: &OperationGuard) -> bool {
        operation.belongs_to(&self.operations)
    }

    fn policy(&self, workflow: WorkflowKind, cancel: HardCancellation) -> Box<dyn ApplyPolicy> {
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

    pub(crate) async fn scribe(
        &self,
        config: &tauqe_core::config::AppConfig,
        operation: &OperationGuard,
        usage: UsageBinding,
    ) -> Result<StructuredExecution, TurnProviderError> {
        if !self.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let model = config.history_model();
        let provider = self.provider(config, model.provider).await?;
        if !self.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(StructuredExecution {
            provider,
            model,
            effort: None,
            session: None,
            max_input_tokens: config.request.max_input_tokens,
            usage: Some(usage),
        })
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
    pub(crate) executor: ReadyPromptExecutor,
    base_session: String,
    session: String,
    current_model: ModelRef,
}

impl ArchitectResearchExecution {
    /// Binding does not reselect the tier or rebuild the frozen repository materials.
    #[allow(dead_code)] // Routes adopt this when their accounting is migrated.
    pub(crate) fn bind_usage(&mut self, usage: UsageBinding) -> Result<(), ExecutionError> {
        self.executor.bind_usage(usage)
    }

    /// Begin a new phase only after the former phase's accounting has drained.
    #[allow(dead_code)] // The plan workflow adopts this at the phase boundary.
    pub(crate) fn rebind_usage(&mut self, usage: UsageBinding) -> Result<(), ExecutionError> {
        self.executor.rebind_usage(usage)
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
        let identity = PromptIdentity::new(
            tier.model.clone(),
            tier.effort,
            Some(ModelRole::Architect),
            self.config().model_alias(&tier.model).map(str::to_owned),
        );
        self.executor
            .rebind(provider, identity, Some(self.session.clone()));
        Ok(())
    }
}

impl PlanExecutionEnvironment {
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
            max_input_tokens: self.config().request.max_input_tokens,
            usage: None,
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
        root: &Path,
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
        let executor = PromptExecutor::new(
            provider,
            Box::new(PlanRefinePromptProtocol::default()),
            PromptIdentity::new(
                tier.model.clone(),
                tier.effort,
                Some(ModelRole::Architect),
                self.config().model_alias(&tier.model).map(str::to_owned),
            ),
            Some(session.clone()),
            self.config().repl.patch_strategy(),
        )
        .with_max_input_tokens(self.config().request.max_input_tokens)
        .bind_ready(root, self.config(), Some(&operation.hard_cancellation()))
        .await
        .map_err(|error| TurnProviderError::new(error.to_string()))?;
        let execution = ArchitectResearchExecution {
            environment: self.clone(),
            resources: resources.clone(),
            executor,
            base_session: session.clone(),
            session,
            current_model: tier.model.clone(),
        };
        if !resources.owns(operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(execution)
    }

    pub(crate) fn models(&self) -> &tauqe_core::config::ModelsConfig {
        &self.environment.snapshot.config.models
    }
    pub(crate) fn config(&self) -> &tauqe_core::config::AppConfig {
        &self.environment.snapshot.config
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
    max_input_tokens: usize,
    usage: Option<UsageBinding>,
}

impl BoundPrompt {
    #[allow(dead_code)] // Routes adopt this when their accounting is migrated.
    pub(crate) fn with_usage(mut self, usage: UsageBinding) -> Self {
        self.usage = Some(usage);
        self
    }

    pub(crate) async fn bind_ready_executor(
        self,
        root: &Path,
        config: &tauqe_core::config::AppConfig,
        cancel: Option<&HardCancellation>,
        identity: PromptIdentity,
        patch_strategy: PatchStrategy,
    ) -> Result<ReadyPromptExecutor, ExecutionError> {
        PromptExecutor::new(
            self.provider,
            self.protocol,
            identity,
            self.session_id,
            patch_strategy,
        )
        .with_max_input_tokens(self.max_input_tokens)
        .bind_ready(root, config, cancel)
        .await
        .and_then(|mut executor| {
            if let Some(usage) = self.usage {
                executor.bind_usage(usage)?;
            }
            Ok(executor)
        })
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
    max_input_tokens: usize,
    usage: Option<UsageBinding>,
}

impl StructuredExecution {
    #[allow(dead_code)] // Routes adopt this when their accounting is migrated.
    pub(crate) fn with_usage(mut self, usage: UsageBinding) -> Self {
        self.usage = Some(usage);
        self
    }

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
            self.max_input_tokens,
            self.usage.as_ref().ok_or_else(|| {
                StructuredError::Failed("structured execution has no usage binding".into())
            })?,
        )
        .await
    }
}

impl<'a> ExecutionFactory<'a> {
    /// Capture the shared dialogue identity once for this interactive executor.
    pub(crate) async fn bind_interactive_prompt(
        &self,
        model: &ModelRef,
        effort: Option<ReasoningEffort>,
        role: Option<ModelRole>,
        patch_strategy: PatchStrategy,
    ) -> Result<ReadyPromptExecutor, TurnProviderError> {
        if !self.application.owns_operation(self.operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let session = format_session_id(
            SESSION_PREFIX_REPL,
            &self.application.context_session().session_uuid(),
        );
        self.bind_prompt(model, effort, role, Some(session), patch_strategy)
            .await
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
        role: Option<ModelRole>,
        session_id: Option<String>,
        patch_strategy: PatchStrategy,
    ) -> Result<ReadyPromptExecutor, TurnProviderError> {
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
        let root = self.application.repository_root().await;
        if !self.application.owns_operation(self.operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        let executor = prompt
            .bind_ready_executor(
                &root,
                &self.environment.snapshot.config,
                Some(&self.operation.hard_cancellation()),
                PromptIdentity::new(
                    model.clone(),
                    effort,
                    role,
                    self.environment
                        .snapshot
                        .config
                        .model_alias(model)
                        .map(str::to_owned),
                ),
                patch_strategy,
            )
            .await
            .map_err(|error| TurnProviderError::new(error.to_string()))?;
        if !self.application.owns_operation(self.operation) {
            return Err(TurnProviderError::foreign_operation());
        }
        Ok(executor)
    }

    pub(crate) fn apply_policy(&self, workflow: WorkflowKind) -> Box<dyn ApplyPolicy> {
        self.application
            .execution_session()
            .policy(workflow, self.operation.hard_cancellation())
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
        let policy = resources.policy(model.config.repl.workflow, operation.hard_cancellation());
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
            max_input_tokens: self.snapshot.config.request.max_input_tokens,
            usage: None,
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
    use crate::api::ContextClearScope;
    use crate::application::configuration::ReloadSelection;
    use tauqe_core::config::{AppConfig, ModelsConfig, OpenRouterConfig, ProvidersConfig};
    use tauqe_core::execution::executor::{ExecuteInput, PromptMessage, RequestRound};
    use tauqe_core::execution::usage::{UsageScope, UsageSink};
    use tauqe_core::llm::step::turn_capability_expr;
    use tauqe_core::models::{ModelEntry, ModelRef};
    use tauqe_core::plan::prompt::architect_synthesis_capability_expr;
    use tauqe_core::prompt::TurnBrief;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::workflow::research::journal::TurnResearchJournal;

    struct NoopUsage;
    impl UsageSink for NoopUsage {
        fn record(&self, _: &tauqe_core::providers::ModelUsageInfo) {}
    }

    fn binding() -> UsageBinding {
        let scope = UsageScope::new(Arc::new(NoopUsage));
        scope.activate();
        UsageBinding::Bound(scope)
    }

    fn request(round: usize) -> ExecuteInput<'static> {
        ExecuteInput {
            message: PromptMessage::Task("Check affinity".into()),
            context_items: vec![],
            history: None,
            capabilities: turn_capability_expr(),
            brief: Some(TurnBrief::step()),
            feedback: None,
            directives: None,
            staged: None,
            created_paths: vec![],
            round: Some(RequestRound {
                round,
                max_rounds: 4,
                reason: None,
            }),
            journal: None,
        }
    }

    fn architect_request(journal: &TurnResearchJournal) -> ExecuteInput<'_> {
        let mut input = request(1);
        input.capabilities = architect_synthesis_capability_expr();
        input.brief = Some(TurnBrief::architect());
        input.round = None;
        input.journal = Some(journal);
        input
    }

    #[tokio::test]
    async fn interactive_executors_snapshot_dialogue_identity_across_rounds() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("transient.rs"), "source").unwrap();
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(config(None), root.path().into(), tx);
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(
                r#"{"message":"first accepted","action":{"kind":"blocked","blocked":{"reason":"test","details":"","is_blocked":true}}}"#,
            ),
            MockResponse::text(
                r#"{"message":"second accepted","action":{"kind":"blocked","blocked":{"reason":"test","details":"","is_blocked":true}}}"#,
            ),
            MockResponse::text(
                r#"{"message":"third accepted","action":{"kind":"blocked","blocked":{"reason":"test","details":"","is_blocked":true}}}"#,
            ),
            MockResponse::text(
                r#"{"message":"fourth accepted","action":{"kind":"blocked","blocked":{"reason":"test","details":"","is_blocked":true}}}"#,
            ),
        ]));
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let guard = app.try_begin_operation().unwrap();
        let factory = ExecutionFactory::bind(&app, &guard).await.unwrap();
        let model = factory.resolve_model(factory.selection().clone(), ModelRole::REPL);
        let bind = || {
            factory.bind_interactive_prompt(
                &model.model,
                model.effort,
                model.role,
                model.config.repl.patch_strategy(),
            )
        };
        let mut first = bind().await.unwrap();
        let mut second = bind().await.unwrap();
        first.bind_usage(binding()).unwrap();
        second.bind_usage(binding()).unwrap();
        for executor in [&mut first, &mut second] {
            executor
                .execute(request(1), &EventSink::noop(), &CancellationToken::never())
                .await
                .unwrap();
        }
        app.mutate_context(|context| {
            context
                .add_auto_file("transient.rs", tauqe_core::context::ContextAccess::ReadOnly)
                .unwrap();
        })
        .await
        .unwrap();
        app.clear_context(ContextClearScope::Transient)
            .await
            .unwrap();
        let mut third = bind().await.unwrap();
        third.bind_usage(binding()).unwrap();
        third
            .execute(request(1), &EventSink::noop(), &CancellationToken::never())
            .await
            .unwrap();
        first
            .execute(
                ExecuteInput {
                    message: PromptMessage::Continuation,
                    ..request(2)
                },
                &EventSink::noop(),
                &CancellationToken::never(),
            )
            .await
            .unwrap();
        let requests = provider.requests();
        let old = requests[0].session_id.as_deref();
        assert!(old.unwrap().starts_with("tauqe-repl-"));
        assert_eq!(requests[1].session_id.as_deref(), old);
        assert_ne!(requests[2].session_id.as_deref(), old);
        assert_eq!(requests[3].session_id.as_deref(), old);
        assert!(requests[1].messages.iter().all(|m| m.role != "assistant"));
        assert!(requests[2].messages.iter().all(|m| m.role != "assistant"));
        assert!(requests[3].messages.starts_with(&requests[0].messages));
        assert!(requests[3]
            .messages
            .iter()
            .any(|m| m.role == "assistant" && m.content == "first accepted"));
        assert!(
            !requests[3]
                .messages
                .iter()
                .any(|m| m.content.contains("second accepted")
                    || m.content.contains("third accepted"))
        );
        assert_eq!(first.history_reader()().len(), 2);
        assert_eq!(second.history_reader()().len(), 1);
        assert_eq!(third.history_reader()().len(), 1);
    }

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
        let executor = factory
            .bind_prompt(
                &model.model,
                model.effort,
                model.role,
                Some("frozen-session".into()),
                model.config.repl.patch_strategy(),
            )
            .await
            .unwrap();
        assert_eq!(executor.identity().model(), &model.model);
        assert_eq!(executor.identity().effort(), model.effort);
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
        app.reset_session_uuid().await;
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
            struct Recorder(std::sync::Mutex<Vec<tauqe_core::providers::ModelUsageInfo>>);
            impl tauqe_core::execution::usage::UsageSink for Recorder {
                fn record(&self, report: &tauqe_core::providers::ModelUsageInfo) {
                    self.0.lock().unwrap().push(report.clone());
                }
            }
            let scope = tauqe_core::execution::usage::UsageScope::new(Arc::new(Recorder(
                std::sync::Mutex::new(Vec::new()),
            )));
            scope.activate();
            let prompt = prompt.with_usage(UsageBinding::Bound(scope));
            let mut executor = prompt
                .bind_ready_executor(
                    &app.repository_root().await,
                    &model.config,
                    None,
                    PromptIdentity::new(
                        model.model.clone(),
                        model.effort,
                        model.role,
                        model.alias.clone(),
                    ),
                    model.config.repl.patch_strategy(),
                )
                .await
                .unwrap();
            assert_eq!(executor.patch_strategy(), PatchStrategy::SearchReplace);
            executor
                .execute(
                    ExecuteInput {
                        message: PromptMessage::Task("Execute the isolated step".into()),
                        ..request(1)
                    },
                    &EventSink::noop(),
                    &CancellationToken::never(),
                )
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
        app.reset_session_uuid().await;
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
            &(router.clone() as Arc<dyn LlmProvider>)
        ));
        assert_eq!(execution.model, tier.model);
        assert_eq!(execution.effort, tier.effort);
        assert_eq!(
            execution.session.as_deref(),
            Some("tauqe-architect-plan-critique0")
        );
        let _ = execution
            .with_usage(binding())
            .run(
                &tauqe_core::review::analysis::post_analysis_contract(),
                vec![ChatMessage::user("Check critique affinity")],
                &EventSink::noop(),
                &CancellationToken::never(),
            )
            .await;
        assert_eq!(
            router.last_request().unwrap().session_id.as_deref(),
            Some("tauqe-architect-plan-critique0")
        );
        let post = frozen
            .for_review_post_analysis(
                &app.execution_session(),
                &guard,
                &tier,
                &ArchitectScope::Review("audit".into()),
            )
            .await
            .unwrap()
            .with_usage(binding());
        app.reset_session_uuid().await;
        let _ = post
            .run(
                &tauqe_core::review::analysis::post_analysis_contract(),
                vec![ChatMessage::user("Check post-analysis affinity")],
                &EventSink::noop(),
                &CancellationToken::never(),
            )
            .await;
        assert_eq!(
            router.last_request().unwrap().session_id.as_deref(),
            Some("tauqe-review-audit")
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
            &(provider.clone() as Arc<dyn LlmProvider>)
        ));
        app.reset_session_uuid().await;
        let _ = tier
            .with_usage(binding())
            .run(
                &tauqe_core::review::analysis::post_analysis_contract(),
                vec![ChatMessage::user("Check reviewer affinity")],
                &EventSink::noop(),
                &CancellationToken::never(),
            )
            .await;
        assert_eq!(
            provider.last_request().unwrap().session_id.as_deref(),
            Some("tauqe-review-audit")
        );
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
            &(provider.clone() as Arc<dyn LlmProvider>)
        ));
        app.reset_session_uuid().await;
        let _ = execution
            .with_usage(binding())
            .run(
                &tauqe_core::review::analysis::post_analysis_contract(),
                vec![ChatMessage::user("Check Scribe affinity")],
                &EventSink::noop(),
                &CancellationToken::never(),
            )
            .await;
        assert_eq!(provider.last_request().unwrap().session_id, None);

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
    async fn scribe_uses_the_supplied_snapshot_and_rejects_foreign_operations() {
        let app = Application::new(config(None));
        let provider = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let mut snapshot = config(None);
        snapshot.models.scribe = Some(ModelRef::openrouter("snapshot/scribe"));
        snapshot.request.max_input_tokens = 1234;
        let resources = app.execution_session();
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        assert!(resources
            .scribe(&snapshot, &foreign, binding())
            .await
            .is_err());

        let operation = app.try_begin_operation().unwrap();
        let execution = resources
            .scribe(&snapshot, &operation, binding())
            .await
            .unwrap();
        assert_eq!(execution.model, ModelRef::openrouter("snapshot/scribe"));
        assert_eq!(execution.effort, None);
        assert_eq!(execution.session, None);
        assert_eq!(execution.max_input_tokens, 1234);
        assert!(execution.usage.is_some());
        assert!(Arc::ptr_eq(
            &execution.provider,
            &(provider as Arc<dyn LlmProvider>)
        ));

        assert!(app.release_operation_registration(operation.id()));
        let replacement = app.try_begin_operation().unwrap();
        assert!(resources
            .scribe(&snapshot, &operation, binding())
            .await
            .is_err());
        assert_eq!(app.active_operation_id(), Some(replacement.id()));
    }

    #[tokio::test]
    async fn architect_affinity_rotates_only_when_the_model_changes() {
        let app = Application::new(config(None));
        let response = || {
            MockResponse::text(
                r#"{"status":"success","explanation":"Affinity checked","reason":null,"context_requests":[],"repo_searches":[],"repo_maps":[],"items":[]}"#,
            )
        };
        let router = Arc::new(MockLlmProvider::scripted([response()]));
        let openai = Arc::new(MockLlmProvider::scripted([response()]));
        app.test_set_provider_for(ProviderKind::OpenRouter, router.clone())
            .await;
        app.test_set_provider_for(ProviderKind::OpenAI, openai.clone())
            .await;
        let guard = app.try_begin_operation().unwrap();
        let frozen = PlanExecutionEnvironment::new(app.configuration().await, &guard);
        let scope = ArchitectScope::Review("audit".into());
        let first = ArchitectTier::from(ModelRef::openrouter("base/model"));
        let mut execution = frozen
            .for_architect_research(
                &app.execution_session(),
                &guard,
                &first,
                &scope,
                &app.repository_root().await,
            )
            .await
            .unwrap();
        assert_eq!(execution.session, "tauqe-review-audit");
        assert_eq!(execution.executor.identity().model(), &first.model);
        execution.bind_usage(binding()).unwrap();
        app.reset_session_uuid().await;
        let journal = TurnResearchJournal::default();
        execution
            .executor
            .execute(
                architect_request(&journal),
                &EventSink::noop(),
                &CancellationToken::never(),
            )
            .await
            .unwrap();
        assert_eq!(
            router.last_request().unwrap().session_id.as_deref(),
            Some("tauqe-review-audit")
        );
        assert_eq!(execution.session, "tauqe-review-audit");
        assert_eq!(
            execution.executor.identity().role(),
            Some(ModelRole::Architect)
        );
        let mut deeper = first.clone();
        deeper.effort = Some(ReasoningEffort::High);
        execution.select_tier(&deeper).await.unwrap();
        assert_eq!(execution.session, "tauqe-review-audit");
        assert_eq!(execution.executor.identity().effort(), deeper.effort);
        let switched = ArchitectTier::from(ModelRef {
            provider: ProviderKind::OpenAI,
            name: "next/model".into(),
        });
        execution.select_tier(&switched).await.unwrap();
        assert_eq!(execution.session, "tauqe-review-audit-tier0");
        app.reset_session_uuid().await;
        execution
            .executor
            .execute(
                architect_request(&journal),
                &EventSink::noop(),
                &CancellationToken::never(),
            )
            .await
            .unwrap();
        assert_eq!(
            openai.last_request().unwrap().session_id.as_deref(),
            Some("tauqe-review-audit-tier0")
        );
        assert_eq!(execution.current_model, switched.model);
        assert_eq!(execution.executor.identity().model(), &switched.model);
        assert_eq!(execution.executor.identity().effort(), switched.effort);
        assert_eq!(
            execution.executor.identity().role(),
            Some(ModelRole::Architect)
        );
    }

    #[tokio::test]
    async fn architect_research_rebinds_usage_only_after_the_previous_window_drains() {
        let app = Application::new(config(None));
        app.test_set_provider_for(ProviderKind::OpenRouter, Arc::new(MockLlmProvider::new()))
            .await;
        let guard = app.try_begin_operation().unwrap();
        let frozen = PlanExecutionEnvironment::new(app.configuration().await, &guard);
        let tier = ArchitectTier::from(ModelRef::openrouter("base/model"));
        let mut execution = frozen
            .for_architect_research(
                &app.execution_session(),
                &guard,
                &tier,
                &ArchitectScope::Plan("plan".into()),
                &app.repository_root().await,
            )
            .await
            .unwrap();
        let old = UsageScope::new(Arc::new(NoopUsage));
        old.activate();
        execution
            .bind_usage(UsageBinding::Bound(old.clone()))
            .unwrap();
        let root = execution.executor.repo_root().unwrap().to_path_buf();
        assert!(execution.rebind_usage(binding()).is_err());
        old.stop_accepting();
        assert!(execution.rebind_usage(binding()).is_err());
        old.drain().await;
        execution.rebind_usage(binding()).unwrap();
        assert_eq!(execution.executor.repo_root().unwrap(), root);
        assert_eq!(execution.executor.identity().model(), &tier.model);
    }
}
