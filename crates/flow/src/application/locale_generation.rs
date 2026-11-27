//! Headless locale synthesis, bounded shutdown, and accounting.
use crate::api::{Application, ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::infrastructure::execution::{
    PlanExecutionEnvironment, StructuredExecution,
};
use crate::workflows::structured::{self, StructuredFailure};
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::locale::{locale_translation_contract, LocaleGenerateParams, LocaleGenerateResult};
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::providers::ChatMessage;

#[derive(Debug, thiserror::Error)]
pub enum LocaleGenerationError {
    #[error("Another operation is already running")]
    Busy,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Provider(String),
    #[error("{0}")]
    Model(String),
    #[error("{0}")]
    Cancelled(String),
}
struct PreparedLocaleGeneration {
    params: LocaleGenerateParams,
}
impl Application {
    pub async fn generate_locale(
        &self,
        params: LocaleGenerateParams,
        observer: Arc<dyn ApplicationObserver>,
        exit_deadline: Duration,
    ) -> Result<LocaleGenerateResult, LocaleGenerationError> {
        let operation = self
            .try_begin_operation()
            .map_err(|_| LocaleGenerationError::Busy)?;
        let prepared = self.prepare_locale_generation(params).await?;
        let environment = PlanExecutionEnvironment::new(self.configuration().await, &operation);
        let execution = environment
            .for_scribe_structured(self, &operation, &ModelSelection::Role(ModelRole::Scribe))
            .await
            .map_err(|error| LocaleGenerationError::Provider(error.to_string()))?;
        self.execute_locale_generation(&operation, prepared, &execution, observer, exit_deadline)
            .await
    }

    async fn prepare_locale_generation(
        &self,
        params: LocaleGenerateParams,
    ) -> Result<PreparedLocaleGeneration, LocaleGenerationError> {
        if params.language.trim().is_empty() {
            return Err(LocaleGenerationError::Invalid(
                "Target language cannot be empty".into(),
            ));
        }
        if params.items.is_empty() {
            return Err(LocaleGenerationError::Invalid(
                "No catalog items to translate".into(),
            ));
        }
        Ok(PreparedLocaleGeneration { params })
    }
    async fn execute_locale_generation(
        &self,
        operation: &OperationGuard,
        prepared: PreparedLocaleGeneration,
        execution: &StructuredExecution,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<LocaleGenerateResult, LocaleGenerationError> {
        if !self.owns_operation(operation) {
            return Err(LocaleGenerationError::Model(
                "locale generation requires an active operation for this session".into(),
            ));
        }
        let PreparedLocaleGeneration { params } = prepared;
        let op_id = format!("op-{}", crate::api::next_operation_id());
        let publisher = Arc::clone(&observer);
        let hard = operation.hard_cancellation();
        let (result, receipt) = structured::run_bound(
            &self.execution_session(),
            self.context_session(),
            operation,
            execution,
            locale_translation_contract(),
            vec![ChatMessage::user(
                tauqe_core::locale::build_locale_user_prompt(&params),
            )],
            &op_id,
            deadline,
            move |id, snapshot| {
                publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()))
            },
            |_| async {},
        )
        .await
        .map_err(|error| {
            LocaleGenerationError::Model(match error.classify("locale generation") {
                StructuredFailure::Failed(message) | StructuredFailure::Decode(message) => message,
                _ => "Locale generation was cancelled".into(),
            })
        })?;
        observer.publish(&op_id, ApplicationEvent::Usage(receipt.snapshot));
        if hard.is_cancelled() {
            return Err(LocaleGenerationError::Cancelled(
                "Locale generation was cancelled".into(),
            ));
        }
        let translations = match result {
            Ok(translations) => translations,
            // Empty is reported only after a clean Done; incomplete or failed
            // requests must not silently publish an English locale.
            Err(error) => match error.classify("locale generation") {
                StructuredFailure::Empty => Default::default(),
                StructuredFailure::InputLimit(error) => {
                    return Err(LocaleGenerationError::Model(error.to_string()));
                }
                StructuredFailure::Cancelled => {
                    return Err(LocaleGenerationError::Cancelled(
                        "Locale generation was cancelled".into(),
                    ));
                }
                StructuredFailure::Failed(message) | StructuredFailure::Decode(message) => {
                    return Err(LocaleGenerationError::Model(message));
                }
            },
        };
        Ok(tauqe_core::locale::validate_and_sanitize_locale(
            &params,
            &translations,
        ))
    }
}
