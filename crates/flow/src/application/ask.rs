//! Interactive admission; workflow ordering belongs to `workflows::repl::run`.
use crate::api::{Application, ApplicationObserver, DiscussionPromptError, ModelAskInput};
use crate::application::infrastructure::execution::ExecutionFactory;
use crate::application::turn_setup::TurnProviderError;
use crate::workflows::repl;
use std::sync::Arc;
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::providers::{format_session_id, SESSION_PREFIX_REPL};

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
        let session = format_session_id(SESSION_PREFIX_REPL, &self.session_uuid().await);
        let executor = factory
            .bind_prompt(
                &model.model,
                model.effort,
                Some(session),
                model.config.repl.patch_strategy(),
            )
            .await?;
        let policy = factory.apply_policy(model.config.repl.workflow);
        let compaction = factory.compaction_environment();
        let application = self.clone();
        tokio::spawn(async move {
            repl::run(
                &application,
                operation,
                prepared,
                model,
                executor,
                policy,
                compaction,
                observer,
            )
            .await;
        });
        Ok(AskStarted { operation_id })
    }
}
