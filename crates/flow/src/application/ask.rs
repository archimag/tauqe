//! Interactive admission; workflow ordering belongs to `workflows::repl::run`.
use crate::api::{Application, ApplicationObserver, DiscussionPromptError, ModelAskInput};
use crate::application::infrastructure::execution::ExecutionFactory;
use crate::application::turn_setup::TurnProviderError;
use crate::workflows::repl::lifecycle;
use std::sync::Arc;
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::workflow::WorkflowOptions;

#[derive(Debug)]
pub struct AskStarted {
    pub operation_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error("Another operation is already running")]
    Busy,
    #[error(transparent)]
    Prompt(#[from] DiscussionPromptError),
    #[error(transparent)]
    Provider(#[from] TurnProviderError),
}

impl Application {
    pub async fn ask(
        &self,
        input: ModelAskInput,
        observer: Arc<dyn ApplicationObserver>,
    ) -> Result<AskStarted, AskError> {
        let operation = Arc::new(self.try_begin_operation().map_err(|_| AskError::Busy)?);
        let root = self.repository_root().await;
        let mut prepared = self.prepare_model_ask(&root, input).await?;
        let operation_id = operation.publication_id();
        if let Some(update) = prepared.focus_update.take() {
            update.publish(&operation_id, observer.as_ref());
        }
        let factory = ExecutionFactory::bind(self, &operation).await?;
        let selection = factory.selection().clone();
        let effort_role = match &selection {
            ModelSelection::Role(role) => *role,
            _ => ModelRole::REPL,
        };
        let model = factory.resolve_model(selection, effort_role);
        let max_turn_rounds = WorkflowOptions::from_app_config(&model.config).max_turn_rounds();
        let scope = self.execution_session().prepare_scope();
        let binding = scope.binding();
        let mut executor = factory
            .bind_interactive_prompt(
                &model.model,
                model.effort,
                model.role,
                model.config.repl.patch_strategy(),
            )
            .await?;
        executor
            .bind_usage(binding.clone())
            .map_err(|error| AskError::Provider(TurnProviderError::new(error.to_string())))?;
        // Both technical bindings are frozen before scheduling the task.
        let policy = factory.apply_policy(model.config.repl.workflow);
        let services = lifecycle::LifecycleServices {
            context: self.context_session().clone(),
            interactive: self.interactive_execution().clone(),
            execution: self.execution_session(),
            history: self.history_session().clone(),
        };
        tokio::spawn(async move {
            lifecycle::run(
                services,
                operation,
                prepared,
                max_turn_rounds,
                Arc::new(model.config),
                scope,
                executor,
                policy,
                observer,
            )
            .await;
        });
        Ok(AskStarted { operation_id })
    }
}
