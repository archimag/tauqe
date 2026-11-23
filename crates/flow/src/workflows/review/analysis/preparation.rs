//! Prepare a review post-analysis round before provider execution.

use super::AnalysisResources;
use crate::api::OperationGuard;
use crate::application::documents::Documents;
use crate::application::PlanExecutionEnvironment;
use std::path::Path;
use tauqe_core::models::{ArchitectTier, ModelRole, ModelSelection};
use tauqe_core::plan::prompt::resolve_architect_tier;
use tauqe_core::providers::ChatMessage;
use tauqe_core::review::analysis::ensure_finding_identities;
use tauqe_core::review::prompt::analysis::build_post_analysis_user_prompt;
use tauqe_core::review::ReviewSession;

pub struct PreparedReviewPostAnalysis {
    pub session: ReviewSession,
    pub tier: ArchitectTier,
    messages: Vec<ChatMessage>,
}

impl PreparedReviewPostAnalysis {
    pub(crate) fn messages(&self) -> Vec<ChatMessage> {
        self.messages.clone()
    }
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

impl AnalysisResources<'_> {
    pub(super) async fn prepare_review_post_analysis(
        &self,
        operation: &OperationGuard,
        environment: &PlanExecutionEnvironment,
        root: &Path,
        session: &ReviewSession,
        selection: Option<&ModelSelection>,
    ) -> Result<PreparedReviewPostAnalysis, ReviewAnalysisPreparationError> {
        if !self.execution.owns(operation) {
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
            Documents::new(root)
                .save_review(&healed)
                .await
                .map_err(ReviewAnalysisPreparationError::Storage)?;
            tracing::info!(
                review_id = healed.id.as_str(),
                minted,
                "review findings healed with UUIDv7 identities before post-analysis"
            );
        }
        // Healing is durable even when model selection or provider setup fails.
        let tier = resolve_post_analysis_architect(environment.config(), selection)
            .map_err(ReviewAnalysisPreparationError::InvalidModel)?;
        let messages = vec![ChatMessage::user(build_post_analysis_user_prompt(&healed))];
        Ok(PreparedReviewPostAnalysis {
            session: healed,
            tier,
            messages,
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
    cfg: &tauqe_core::config::AppConfig,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use tauqe_core::config::{AppConfig, ModelsConfig};
    use tauqe_core::models::{ModelEntry, ModelRef, ReasoningEffort};
    use tauqe_core::review::ReviewStorage;

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

    async fn environment(
        app: &Application,
        operation: &OperationGuard,
    ) -> PlanExecutionEnvironment {
        PlanExecutionEnvironment::new(app.configuration().await, operation)
    }

    #[tokio::test]
    async fn healing_is_durable_before_an_invalid_selection_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let environment = environment(&app, &operation).await;
        let review = session();
        let selection = ModelSelection::Specific(ModelRef::openrouter("test/unknown"));
        let result = app
            .audit_resources()
            .analysis()
            .prepare_review_post_analysis(
                &operation,
                &environment,
                root.path(),
                &review,
                Some(&selection),
            )
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
        let environment = environment(&app, &operation).await;
        let selection = ModelSelection::Specific(ModelRef::openrouter("test/deep"));
        let prepared = app
            .audit_resources()
            .analysis()
            .prepare_review_post_analysis(
                &operation,
                &environment,
                root.path(),
                &session(),
                Some(&selection),
            )
            .await
            .unwrap();
        assert_eq!(prepared.tier.model, ModelRef::openrouter("test/deep"));
        assert_eq!(prepared.tier.effort, Some(ReasoningEffort::High));
        let stored = ReviewStorage::load_review(root.path(), &prepared.session.id)
            .unwrap()
            .unwrap();
        assert_eq!(prepared.session.items[0].uuid, stored.items[0].uuid);
        assert_eq!(prepared.messages[0].role, "user");
        assert!(prepared.messages[0]
            .content
            .contains(stored.items[0].uuid.as_deref().unwrap()));
        let base = app
            .audit_resources()
            .analysis()
            .prepare_review_post_analysis(&operation, &environment, root.path(), &stored, None)
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
        let environment = environment(&app, &operation).await;
        let result = app
            .audit_resources()
            .analysis()
            .prepare_review_post_analysis(
                &operation,
                &environment,
                root.path(),
                &ReviewSession::default(),
                None,
            )
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPreparationError::EmptyFindings(_))
        ));
        let other = Application::new(config());
        let foreign = other.try_begin_operation().unwrap();
        let result = app
            .audit_resources()
            .analysis()
            .prepare_review_post_analysis(&foreign, &environment, root.path(), &session(), None)
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
        let environment = environment(&app, &operation).await;
        let selection = ModelSelection::Specific(ModelRef::openrouter("test/unknown"));
        let result = app
            .audit_resources()
            .analysis()
            .prepare_review_post_analysis(
                &operation,
                &environment,
                root.path(),
                &session(),
                Some(&selection),
            )
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisPreparationError::Storage(_))
        ));
    }

    #[tokio::test]
    async fn preparation_uses_the_bound_configuration_snapshot() {
        use crate::api::ReloadSelection;
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config());
        let operation = app.try_begin_operation().unwrap();
        let frozen = environment(&app, &operation).await;
        let mut changed = config();
        changed.models.architect = Some(vec![ModelEntry::new(
            ModelRef::openrouter("test/new"),
            None,
        )]);
        app.replace_configuration(changed, ReloadSelection::Preserve)
            .await;
        let prepared = app
            .audit_resources()
            .analysis()
            .prepare_review_post_analysis(&operation, &frozen, root.path(), &session(), None)
            .await
            .unwrap();
        assert_eq!(prepared.tier.model, ModelRef::openrouter("test/base"));
    }
}
