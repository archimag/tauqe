//! Build and start the single-audit request from explicit frozen inputs.

use crate::api::{Application, OperationGuard, ResolvedReviewSynthesis};

use std::time::{SystemTime, UNIX_EPOCH};
use tauqe_core::context::ContextFileContent;
use tauqe_core::providers::ChatMessage;
use tauqe_core::review::build_review_user_prompt;
use tauqe_core::review::synthesis::ReviewSynthesisMetadata;

#[derive(Debug, thiserror::Error)]
#[error("provider execution requires an active operation for this session")]
pub(crate) struct ReviewSynthesisRequestError;

pub struct PreparedReviewSynthesisRequest {
    metadata: ReviewSynthesisMetadata,
    prompt: String,
}
impl PreparedReviewSynthesisRequest {
    pub fn id(&self) -> &str {
        &self.metadata.id
    }

    pub fn into_parts(self) -> (ReviewSynthesisMetadata, Vec<ChatMessage>) {
        (self.metadata, vec![ChatMessage::user(self.prompt)])
    }
}

impl Application {
    pub(crate) fn prepare_review_synthesis_request(
        &self,
        operation: &OperationGuard,
        resolved: &ResolvedReviewSynthesis,
        files: &[ContextFileContent],
        user_prompt: Option<String>,
    ) -> Result<PreparedReviewSynthesisRequest, ReviewSynthesisRequestError> {
        if !self.owns_operation(operation) {
            return Err(ReviewSynthesisRequestError);
        }
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = tauqe_core::review::uuid::generate_uuid_v7();
        let prompt = build_review_user_prompt(files, user_prompt.as_deref());
        let metadata = ReviewSynthesisMetadata {
            id,
            created_at,
            model: resolved.model.clone(),
            user_prompt,
            target_files: files.iter().map(|file| file.path.clone()).collect(),
        };
        Ok(PreparedReviewSynthesisRequest { metadata, prompt })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::{AppConfig, ModelsConfig};
    use tauqe_core::context::ContextAccess;
    use tauqe_core::models::{ModelEntry, ModelRef, ReasoningEffort};

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
    async fn request_freezes_prompt_and_review_identity() {
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
        let (metadata, messages) = prepared.into_parts();
        assert!(tauqe_core::review::is_valid_uuid_v7(&metadata.id));
        assert!(metadata.created_at > 0);
        assert_eq!(metadata.model, resolved.model);
        assert_eq!(metadata.target_files, ["audit.rs"]);
        assert_eq!(metadata.user_prompt.as_deref(), Some("Audit boundaries"));
        assert_eq!(messages[0].role, "user");
        assert!(messages[0].content.contains("source before launch"));
        assert!(messages[0].content.contains("Audit boundaries"));
        assert!(!messages[0].content.contains("changed after launch"));
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn foreign_operation_cannot_prepare_a_request() {
        let app = application();
        let _operation = app.try_begin_operation().unwrap();
        let resolved = app.resolve_review_synthesis(None).await;
        let other = application();
        let foreign = other.try_begin_operation().unwrap();
        assert!(app
            .prepare_review_synthesis_request(&foreign, &resolved, &files(), None)
            .is_err());
    }
}
