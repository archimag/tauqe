//! Prepare a review post-analysis round before provider execution.

use crate::api::{Application, OperationGuard};
use std::path::Path;
use tauqe_core::config::AppConfig;
use tauqe_core::models::{ArchitectTier, ModelRole, ModelSelection};
use tauqe_core::plan::prompt::resolve_architect_tier;
use tauqe_core::providers::{
    format_session_id, ChatMessage, ResponseFormat, SESSION_PREFIX_REVIEW,
};
use tauqe_core::review::analysis::{ensure_finding_identities, post_analysis_response_format};
use tauqe_core::review::prompt::analysis::{
    build_post_analysis_system_prompt, build_post_analysis_user_prompt,
};
use tauqe_core::review::ReviewSession;
use tauqe_core::review::ReviewStorage;

pub struct PreparedReviewPostAnalysis {
    pub session: ReviewSession,
    pub(crate) config: AppConfig,
    pub tier: ArchitectTier,
    messages: Vec<ChatMessage>,
    response_format: ResponseFormat,
    session_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewAnalysisPreparationError {
    #[error("review post-analysis requires an active operation for this session")]
    OperationNotOwned,
    #[error("Review session '{0}' has no findings to classify; run the audit first")]
    EmptyFindings(String),
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    InvalidModel(String),
}

impl Application {
    pub(crate) async fn prepare_review_post_analysis(
        &self,
        operation: &OperationGuard,
        root: &Path,
        session: &ReviewSession,
        selection: Option<&ModelSelection>,
    ) -> Result<PreparedReviewPostAnalysis, ReviewAnalysisPreparationError> {
        if !self.owns_operation(operation) {
            return Err(ReviewAnalysisPreparationError::OperationNotOwned);
        }
        if session.items.is_empty() {
            return Err(ReviewAnalysisPreparationError::EmptyFindings(
                session.id.clone(),
            ));
        }
        let mut healed = session.clone();
        let minted = ensure_finding_identities(&mut healed);
        if minted > 0 {
            let save_root = root.to_path_buf();
            let to_save = healed.clone();
            // Preserve the original offload's failure handling: an interrupted
            // or panicking write returns an error without retrying a mutation.
            tokio::task::spawn_blocking(move || ReviewStorage::save_session(&save_root, &to_save))
                .await
                .map_err(|error| {
                    ReviewAnalysisPreparationError::Storage(format!(
                        "blocking task failed: {error}"
                    ))
                })?
                .map_err(|error| ReviewAnalysisPreparationError::Storage(format!("{error:#}")))?;
            tracing::info!(
                review_id = healed.id.as_str(),
                minted,
                "review findings healed with UUIDv7 identities before post-analysis"
            );
        }
        // Healing is durable even when model selection or provider setup fails.
        let config = self.configuration().await.config;
        let tier = resolve_post_analysis_architect(&config, selection)
            .map_err(ReviewAnalysisPreparationError::InvalidModel)?;
        let messages = vec![
            ChatMessage::system(build_post_analysis_system_prompt()),
            ChatMessage::user(build_post_analysis_user_prompt(&healed)),
        ];
        let session_id = format_session_id(SESSION_PREFIX_REVIEW, &healed.id);
        Ok(PreparedReviewPostAnalysis {
            session: healed,
            config,
            tier,
            messages,
            response_format: post_analysis_response_format(),
            session_id,
        })
    }
}

/// Resolves the Architect configuration of one post-analysis round.
///
/// Tier resolution is the same core-owned policy plan refinement runs on, so
/// the caller receives the configuration the policy returned and never derives
/// the architect ladder a second time. An absent selection keeps the base rung,
/// while an explicit one carries the model together with the reasoning depth
/// of the configured entry the developer picked.
///
/// A selection naming a model the architect role does not carry is refused by
/// name. Silently falling back to the base rung would classify the findings
/// with a configuration nobody asked for, which is exactly what an explicit
/// selection exists to prevent. A role selection resolves through the domain
/// role chain, so it never needs the membership check.
fn resolve_post_analysis_architect(
    cfg: &AppConfig,
    selection: Option<&ModelSelection>,
) -> Result<ArchitectTier, String> {
    let Some(selection) = selection else {
        return Ok(resolve_architect_tier(cfg, None, None, None, None).0);
    };
    if !matches!(selection, ModelSelection::Role(_)) {
        let requested = cfg.resolve_model(selection);
        let configured = cfg
            .models
            .role_entries(ModelRole::Architect)
            .iter()
            .any(|entry| entry.model == requested);
        if !configured {
            return Err(format!(
                "Architect selection '{}' is not a configured architect entry",
                selection
            ));
        }
    }
    Ok(resolve_architect_tier(cfg, None, Some(selection), None, None).0)
}

/// Existing post-analysis task API backed by shared provider execution.
pub use crate::application::provider_task::ProviderTask as ReviewAnalysisTask;

impl Application {
    pub(crate) fn start_review_post_analysis<'a>(
        &self,
        operation: &'a OperationGuard,
        prepared: &PreparedReviewPostAnalysis,
        provider: std::sync::Arc<dyn tauqe_core::providers::LlmProvider>,
    ) -> Result<ReviewAnalysisTask<'a>, ReviewAnalysisPreparationError> {
        let request = crate::application::provider_task::ProviderRequest {
            model_name: prepared.tier.model.name.clone(),
            messages: prepared.messages.clone(),
            response_format: prepared.response_format.clone(),
            effort: prepared.tier.effort,
            session_id: prepared.session_id.clone(),
        };
        self.start_provider_task(operation, request, provider, "review-post-analysis-stream")
            .map_err(|_| ReviewAnalysisPreparationError::OperationNotOwned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::ModelsConfig;
    use tauqe_core::models::{ModelEntry, ModelRef, ReasoningEffort};

    fn config() -> AppConfig {
        AppConfig {
            models: ModelsConfig {
                architect: Some(vec![
                    ModelEntry::new(
                        ModelRef::openrouter("test/base"),
                        Some(ReasoningEffort::Low),
                    ),
                    ModelEntry::new(
                        ModelRef::openrouter("test/deep"),
                        Some(ReasoningEffort::High),
                    ),
                ]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn session() -> ReviewSession {
        ReviewSession {
            id: "preparation-review".into(),
            title: "Preparation audit".into(),
            items: vec![serde_json::from_value(serde_json::json!({
                "id": 1, "title": "Missing boundary", "type": "architecture",
                "body": "One shared boundary is needed"
            }))
            .unwrap()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn healing_is_durable_before_an_invalid_selection_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let review = session();
        let selection = ModelSelection::Specific(ModelRef::openrouter("test/unknown"));
        let result = app
            .prepare_review_post_analysis(&operation, root.path(), &review, Some(&selection))
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPreparationError::InvalidModel(_))
        ));
        let stored = ReviewStorage::load_review(root.path(), &review.id)
            .unwrap()
            .unwrap();
        assert!(tauqe_core::review::is_valid_uuid_v7(
            stored.items[0].uuid.as_deref().unwrap()
        ));
        assert!(review.items[0].uuid.is_none());
    }

    #[tokio::test]
    async fn prepared_round_uses_selected_effort_and_durable_identities_in_prompts() {
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let selection = ModelSelection::Specific(ModelRef::openrouter("test/deep"));
        let prepared = app
            .prepare_review_post_analysis(&operation, root.path(), &session(), Some(&selection))
            .await
            .unwrap();
        assert_eq!(prepared.tier.model, ModelRef::openrouter("test/deep"));
        assert_eq!(prepared.tier.effort, Some(ReasoningEffort::High));
        assert_eq!(prepared.session_id, "tauqe-review-preparation-review");
        let stored = ReviewStorage::load_review(root.path(), &prepared.session.id)
            .unwrap()
            .unwrap();
        assert_eq!(prepared.session.items[0].uuid, stored.items[0].uuid);
        assert_eq!(prepared.messages[0].role, "system");
        assert_eq!(prepared.messages[1].role, "user");
        assert!(prepared.messages[1]
            .content
            .contains(stored.items[0].uuid.as_deref().unwrap()));
        let base = app
            .prepare_review_post_analysis(&operation, root.path(), &stored, None)
            .await
            .unwrap();
        assert_eq!(base.tier.model, ModelRef::openrouter("test/base"));
        assert_eq!(base.tier.effort, Some(ReasoningEffort::Low));
        assert_eq!(base.session.items[0].uuid, stored.items[0].uuid);
    }

    #[tokio::test]
    async fn empty_input_and_foreign_operation_are_refused_before_writes() {
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let result = app
            .prepare_review_post_analysis(&operation, root.path(), &ReviewSession::default(), None)
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPreparationError::EmptyFindings(_))
        ));
        let other = Application::new(config());
        let foreign = other.try_begin_operation().unwrap();
        let result = app
            .prepare_review_post_analysis(&foreign, root.path(), &session(), None)
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPreparationError::OperationNotOwned)
        ));
        assert!(!root.path().join(".tauqe").exists());
    }

    #[tokio::test]
    async fn failed_identity_write_precedes_model_selection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".tauqe"), "blocks the state directory").unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let selection = ModelSelection::Specific(ModelRef::openrouter("test/unknown"));
        let result = app
            .prepare_review_post_analysis(&operation, root.path(), &session(), Some(&selection))
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPreparationError::Storage(_))
        ));
    }

    #[tokio::test]
    async fn execution_uses_the_frozen_request_and_rejects_foreign_operations() {
        use tauqe_core::providers::{mock::MockResponse, MockLlmProvider, StreamEvent};
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let prepared = app
            .prepare_review_post_analysis(&operation, root.path(), &session(), None)
            .await
            .unwrap();
        let provider =
            std::sync::Arc::new(MockLlmProvider::scripted([MockResponse::text("verdict")]));
        let foreign_app = Application::new(config());
        let foreign = foreign_app.try_begin_operation().unwrap();
        assert!(matches!(
            app.start_review_post_analysis(&foreign, &prepared, provider.clone()),
            Err(ReviewAnalysisPreparationError::OperationNotOwned)
        ));
        assert!(provider.requests().is_empty());
        let mut task = app
            .start_review_post_analysis(&operation, &prepared, provider.clone())
            .unwrap();
        let mut text = String::new();
        while let Some(event) = task.next_event().await {
            match event {
                StreamEvent::TextDelta(delta) => text.push_str(&delta),
                StreamEvent::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "verdict");
        let request = provider.last_request().unwrap();
        assert_eq!(request.model, "test/base");
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(
            request.session_id.as_deref(),
            Some(prepared.session_id.as_str())
        );
        assert_eq!(request.messages, prepared.messages);
        assert!(request.response_format.is_some());
        task.clear_abort_handle();
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    struct FailingProvider;

    #[async_trait::async_trait]
    impl tauqe_core::providers::LlmProvider for FailingProvider {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }
        async fn stream(
            &self,
            _request: tauqe_core::providers::LlmRequest<'_>,
            _tx: tokio::sync::mpsc::Sender<tauqe_core::providers::StreamEvent>,
            _cancel: tokio::sync::watch::Receiver<bool>,
        ) -> tauqe_core::providers::ProviderResult {
            Err(tauqe_core::providers::ProviderError::Request(
                anyhow::anyhow!("provider execution failed"),
            ))
        }
    }

    #[tokio::test]
    async fn provider_returned_errors_reach_the_event_stream() {
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let prepared = app
            .prepare_review_post_analysis(&operation, root.path(), &session(), None)
            .await
            .unwrap();
        let mut task = app
            .start_review_post_analysis(&operation, &prepared, std::sync::Arc::new(FailingProvider))
            .unwrap();
        assert!(
            matches!(task.next_event().await, Some(tauqe_core::providers::StreamEvent::Error(message)) if message == "provider execution failed")
        );
        task.clear_abort_handle();
        assert_eq!(
            task.abort_and_reap("op-test", std::time::Duration::from_secs(1))
                .await,
            crate::api::TaskExit::Confirmed
        );
    }

    struct ParkedProvider {
        started: std::sync::Arc<tokio::sync::Notify>,
        exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    struct ProviderExit(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for ProviderExit {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl tauqe_core::providers::LlmProvider for ParkedProvider {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }
        async fn stream(
            &self,
            _request: tauqe_core::providers::LlmRequest<'_>,
            _tx: tokio::sync::mpsc::Sender<tauqe_core::providers::StreamEvent>,
            _cancel: tokio::sync::watch::Receiver<bool>,
        ) -> tauqe_core::providers::ProviderResult {
            let _exit = ProviderExit(self.exited.clone());
            self.started.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn registered_abort_stops_provider_before_the_operation_is_released() {
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let prepared = app
            .prepare_review_post_analysis(&operation, root.path(), &session(), None)
            .await
            .unwrap();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = std::sync::Arc::new(ParkedProvider {
            started: started.clone(),
            exited: exited.clone(),
        });
        let mut task = app
            .start_review_post_analysis(&operation, &prepared, provider)
            .unwrap();
        started.notified().await;
        assert!(app.abort_operation(operation.id()));
        assert!(task.next_event().await.is_none());
        task.clear_abort_handle();
        assert_eq!(
            task.abort_and_reap("op-test", std::time::Duration::from_secs(1))
                .await,
            crate::api::TaskExit::Confirmed
        );
        assert!(exited.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn provider_construction_uses_the_prepared_configuration_snapshot() {
        use crate::api::ReloadSelection;
        use tauqe_core::config::OpenRouterConfig;
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let without_key = app
            .prepare_review_post_analysis(&operation, root.path(), &session(), None)
            .await
            .unwrap();
        let mut configured = config();
        configured.providers.openrouter = Some(OpenRouterConfig {
            api_key: Some("test-key".into()),
            ..Default::default()
        });
        app.replace_configuration(configured, ReloadSelection::Preserve)
            .await;
        assert!(app
            .resolve_provider(without_key.tier.model.provider, &without_key.config)
            .await
            .is_err());
        let with_key = app
            .prepare_review_post_analysis(&operation, root.path(), &session(), None)
            .await
            .unwrap();
        app.replace_configuration(config(), ReloadSelection::Preserve)
            .await;
        assert!(app
            .resolve_provider(with_key.tier.model.provider, &with_key.config)
            .await
            .is_ok());
        assert!(app
            .resolve_provider(without_key.tier.model.provider, &without_key.config)
            .await
            .is_err());
    }
}
