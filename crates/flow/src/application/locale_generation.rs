//! Headless locale synthesis, bounded shutdown, and accounting.
use crate::api::{Application, ApplicationEvent, ApplicationObserver, OperationGuard, UsageGuard};
use crate::application::infrastructure::execution::{
    PlanExecutionEnvironment, StructuredExecution,
};
use crate::application::infrastructure::structured_task::{self, StructuredTaskError};
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::control::structured::StructuredError;
use tauqe_core::locale::{locale_translation_contract, LocaleGenerateParams, LocaleGenerateResult};
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::providers::{ChatMessage, StreamEvent};

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
        let usage_guard = self
            .begin_usage(&op_id, move |id, snapshot| {
                publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()))
            })
            .await;
        let cancel_rx = operation.cancel_receiver();
        let result = structured_task::run(
            self,
            operation,
            execution,
            locale_translation_contract(),
            vec![ChatMessage::user(
                tauqe_core::locale::build_locale_user_prompt(&params),
            )],
            &op_id,
            deadline,
            |event| {
                let usage = &usage_guard;
                async move {
                    if let StreamEvent::Usage(reported) = event {
                        usage.observe(&reported).await;
                    }
                }
            },
        )
        .await;
        close_locale_usage(self, usage_guard, &op_id, &observer).await;
        if *cancel_rx.borrow() {
            return Err(LocaleGenerationError::Cancelled(
                "Locale generation was cancelled".into(),
            ));
        }
        let translations = match result {
            Ok(translations) => translations,
            // Empty is reported only after a clean Done; incomplete or failed
            // requests must not silently publish an English locale.
            Err(StructuredTaskError::Request(StructuredError::Empty)) => Default::default(),
            Err(StructuredTaskError::Cancelled)
            | Err(StructuredTaskError::Request(StructuredError::Cancelled)) => {
                return Err(LocaleGenerationError::Cancelled(
                    "Locale generation was cancelled".into(),
                ))
            }
            Err(StructuredTaskError::Stream(message))
            | Err(StructuredTaskError::Task(message))
            | Err(StructuredTaskError::Request(StructuredError::Failed(message)))
            | Err(StructuredTaskError::Request(StructuredError::Decode(message))) => {
                return Err(LocaleGenerationError::Model(message))
            }
            Err(StructuredTaskError::Request(StructuredError::Incomplete)) => {
                return Err(LocaleGenerationError::Model(
                    "Structured request closed without Done".into(),
                ))
            }
            Err(StructuredTaskError::ForeignOperation) => {
                return Err(LocaleGenerationError::Model(
                    "locale generation requires an active operation for this session".into(),
                ))
            }
            Err(StructuredTaskError::Orphaned) => {
                return Err(LocaleGenerationError::Model(format!(
                    "Locale generation stream task did not stop within the {} ms task-exit deadline",
                    deadline.as_millis()
                )))
            }
        };
        Ok(tauqe_core::locale::validate_and_sanitize_locale(
            &params,
            &translations,
        ))
    }
}
async fn close_locale_usage(
    application: &Application,
    guard: UsageGuard,
    id: &str,
    observer: &Arc<dyn ApplicationObserver>,
) {
    guard.flush().await;
    observer.publish(
        id,
        ApplicationEvent::Usage(application.usage_snapshot().await),
    );
}
