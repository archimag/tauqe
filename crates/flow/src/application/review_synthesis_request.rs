//! Build and start the single-audit request from explicit frozen inputs.

use crate::api::{
    Application, OperationGuard, ProviderTask, ProviderTaskError, ResolvedReviewSynthesis,
};
use crate::application::provider_task::ProviderRequest;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tauqe_core::context::ContextFileContent;
use tauqe_core::providers::{format_session_id, ChatMessage, LlmProvider, SESSION_PREFIX_REVIEW};
use tauqe_core::review::synthesis::ReviewSynthesisMetadata;
use tauqe_core::review::{
    build_review_system_prompt, build_review_user_prompt, review_findings_response_format,
};

pub struct PreparedReviewSynthesisRequest {
    metadata: ReviewSynthesisMetadata,
    request: ProviderRequest,
}
impl PreparedReviewSynthesisRequest {
    pub fn into_metadata(self) -> ReviewSynthesisMetadata {
        self.metadata
    }
}

impl Application {
    pub(crate) fn prepare_review_synthesis_request(
        &self,
        operation: &OperationGuard,
        resolved: &ResolvedReviewSynthesis,
        files: &[ContextFileContent],
        user_prompt: Option<String>,
    ) -> Result<PreparedReviewSynthesisRequest, ProviderTaskError> {
        if !self.owns_operation(operation) {
            return Err(ProviderTaskError);
        }
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = tauqe_core::review::uuid::generate_uuid_v7();
        let request = ProviderRequest {
            model_name: resolved.model.name.clone(),
            messages: vec![
                ChatMessage::system(build_review_system_prompt()),
                ChatMessage::user(build_review_user_prompt(files, user_prompt.as_deref())),
            ],
            response_format: review_findings_response_format(),
            effort: resolved.effort,
            session_id: format_session_id(SESSION_PREFIX_REVIEW, &id),
        };
        let metadata = ReviewSynthesisMetadata {
            id,
            created_at,
            model: resolved.model.clone(),
            user_prompt,
            target_files: files.iter().map(|file| file.path.clone()).collect(),
        };
        Ok(PreparedReviewSynthesisRequest { metadata, request })
    }

    pub(crate) fn start_review_synthesis<'a>(
        &self,
        operation: &'a OperationGuard,
        prepared: &PreparedReviewSynthesisRequest,
        provider: Arc<dyn LlmProvider>,
    ) -> Result<ProviderTask<'a>, ProviderTaskError> {
        self.start_provider_task(
            operation,
            prepared.request.clone(),
            provider,
            "review-synthesis-stream",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::{AppConfig, ModelsConfig};
    use tauqe_core::context::ContextAccess;
    use tauqe_core::models::{ModelEntry, ModelRef, ReasoningEffort};
    use tauqe_core::providers::{mock::MockResponse, MockLlmProvider, ResponseFormat, StreamEvent};

    fn application() -> Application {
        Application::new(AppConfig {
            models: ModelsConfig {
                reviewer: Some(vec![ModelEntry::new(
                    ModelRef::openrouter("test/reviewer"),
                    Some(ReasoningEffort::High),
                )]),
                ..Default::default()
            },
            ..Default::default()
        })
    }
    fn files() -> Vec<ContextFileContent> {
        vec![ContextFileContent {
            path: "audit.rs".into(),
            content: "source before launch".into(),
            access: ContextAccess::ReadOnly,
            insertion_seq: 7,
        }]
    }

    #[tokio::test]
    async fn request_freezes_messages_schema_and_review_cache_identity() {
        let app = application();
        let operation = app.try_begin_operation().unwrap();
        let resolved = app.resolve_review_synthesis(None).await;
        let mut files = files();
        let prepared = app
            .prepare_review_synthesis_request(
                &operation,
                &resolved,
                &files,
                Some("Audit boundaries".into()),
            )
            .unwrap();
        files[0].content = "changed after launch".into();
        files[0].path = "changed.rs".into();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text("result")]));
        let mut task = app
            .start_review_synthesis(&operation, &prepared, provider.clone())
            .unwrap();
        let mut text = String::new();
        while let Some(event) = task.next_event().await {
            match event {
                StreamEvent::TextDelta(delta) => text.push_str(&delta),
                StreamEvent::Done => break,
                _ => {}
            }
        }
        task.clear_abort_handle();
        assert_eq!(text, "result");
        let metadata = prepared.into_metadata();
        assert!(tauqe_core::review::is_valid_uuid_v7(&metadata.id));
        assert!(metadata.created_at > 0);
        assert_eq!(metadata.model, resolved.model);
        assert_eq!(metadata.target_files, ["audit.rs"]);
        assert_eq!(metadata.user_prompt.as_deref(), Some("Audit boundaries"));
        let request = provider.last_request().unwrap();
        assert_eq!(request.model, "test/reviewer");
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(
            request.session_id,
            Some(format_session_id(SESSION_PREFIX_REVIEW, &metadata.id))
        );
        assert_eq!(request.messages[0].role, "system");
        assert_eq!(request.messages[1].role, "user");
        assert!(request.messages[1].content.contains("source before launch"));
        assert!(request.messages[1].content.contains("Audit boundaries"));
        assert!(!request.messages[1].content.contains("changed after launch"));
        assert!(
            matches!(request.response_format, Some(ResponseFormat::JsonSchema { json_schema }) if json_schema.strict == Some(true))
        );
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn foreign_operation_can_neither_prepare_nor_start_a_request() {
        let app = application();
        let operation = app.try_begin_operation().unwrap();
        let resolved = app.resolve_review_synthesis(None).await;
        let other = application();
        let foreign = other.try_begin_operation().unwrap();
        assert!(app
            .prepare_review_synthesis_request(&foreign, &resolved, &files(), None)
            .is_err());
        let prepared = app
            .prepare_review_synthesis_request(&operation, &resolved, &files(), None)
            .unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text("unused")]));
        assert!(app
            .start_review_synthesis(&foreign, &prepared, provider.clone())
            .is_err());
        assert!(provider.requests().is_empty());
    }
}
