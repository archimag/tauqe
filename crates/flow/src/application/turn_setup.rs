//! Model resolution, provider construction, and runtime policy of a turn.

use crate::api::Application;
#[cfg(test)]
use crate::application::infrastructure::execution::ExecutionEnvironment;
#[cfg(test)]
use std::sync::Arc;
use tauqe_core::config::AppConfig;
use tauqe_core::models::{ModelRef, ModelRole, ModelSelection, ReasoningEffort};
#[cfg(test)]
use tauqe_core::providers::LlmProvider;

pub struct ResolvedTurnModel {
    pub config: AppConfig,
    pub selection: ModelSelection,
    pub model: ModelRef,
    pub role: Option<ModelRole>,
    pub effort: Option<ReasoningEffort>,
    pub alias: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TurnProviderError(String);

impl TurnProviderError {
    pub(crate) fn new(message: String) -> Self {
        Self(message)
    }

    pub(crate) fn foreign_operation() -> Self {
        Self("Execution requires an active operation for this session".into())
    }
}

impl Application {
    #[cfg(test)]
    async fn resolve_turn_provider(
        &self,
        model: &ResolvedTurnModel,
    ) -> Result<Arc<dyn LlmProvider>, TurnProviderError> {
        ExecutionEnvironment::new(crate::application::configuration::ConfigurationSnapshot {
            config: model.config.clone(),
            selection: model.selection.clone(),
        })
        .provider(self, model.model.provider)
        .await
    }

    #[cfg(test)]
    pub(crate) async fn resolve_turn_model(
        &self,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        effort_role: ModelRole,
    ) -> ResolvedTurnModel {
        ExecutionEnvironment::new(self.configuration().await).resolve_model(
            selection,
            effort,
            effort_role,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::{ModelsConfig, OpenRouterConfig, ProvidersConfig};
    use tauqe_core::models::{ModelCatalogEntry, ModelEntry};

    fn config(key: Option<&str>) -> AppConfig {
        let model = ModelRef::openrouter("shared/model");
        AppConfig {
            models: ModelsConfig {
                repl: vec![ModelEntry::new(model.clone(), Some(ReasoningEffort::Low))],
                coder: Some(vec![ModelEntry::new(
                    model.clone(),
                    Some(ReasoningEffort::High),
                )]),
                architect: Some(vec![ModelEntry::new(
                    model.clone(),
                    Some(ReasoningEffort::Max),
                )]),
                ..Default::default()
            },
            providers: ProvidersConfig {
                openai: None,
                openrouter: Some(OpenRouterConfig {
                    api_key: key.map(str::to_string),
                    models: vec![ModelCatalogEntry {
                        model,
                        alias: Some("Shared".into()),
                    }],
                }),
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn active_selection_and_catalog_metadata_come_from_one_snapshot() {
        let app = Application::new(config(None));
        app.begin_configuration_change()
            .unwrap()
            .select_model(ModelSelection::Role(ModelRole::Architect))
            .await
            .unwrap();
        let resolved = app
            .resolve_turn_model(None, None, ModelRole::Architect)
            .await;
        assert_eq!(
            resolved.selection,
            ModelSelection::Role(ModelRole::Architect)
        );
        assert_eq!(resolved.model, ModelRef::openrouter("shared/model"));
        assert_eq!(resolved.role, Some(ModelRole::Architect));
        assert_eq!(resolved.effort, Some(ReasoningEffort::Max));
        assert_eq!(resolved.alias.as_deref(), Some("Shared"));
    }

    #[tokio::test]
    async fn explicit_model_uses_the_calling_contours_effort_defaults() {
        let app = Application::new(config(None));
        let selection = ModelSelection::Specific(ModelRef::openrouter("shared/model"));
        let interactive = app
            .resolve_turn_model(Some(selection.clone()), None, ModelRole::REPL)
            .await;
        let step = app
            .resolve_turn_model(Some(selection), None, ModelRole::Coder)
            .await;
        assert_eq!(interactive.role, None);
        assert_eq!(step.role, None);
        assert_eq!(interactive.effort, Some(ReasoningEffort::Low));
        assert_eq!(step.effort, Some(ReasoningEffort::High));
    }

    #[tokio::test]
    async fn explicit_effort_precedes_tier_and_role_effort() {
        let app = Application::new(config(None));
        for role in [ModelRole::REPL, ModelRole::Coder] {
            let selection = ModelSelection::Tier {
                model: ModelRef::openrouter("shared/model"),
                effort: Some(ReasoningEffort::Max),
            };
            let tier = app
                .resolve_turn_model(Some(selection.clone()), None, role)
                .await;
            assert_eq!(tier.role, None);
            assert_eq!(tier.effort, Some(ReasoningEffort::Max));
            let explicit = app
                .resolve_turn_model(Some(selection), Some(ReasoningEffort::Medium), role)
                .await;
            assert_eq!(explicit.effort, Some(ReasoningEffort::Medium));
        }
    }

    #[tokio::test]
    async fn provider_construction_keeps_the_credentials_of_the_resolved_snapshot() {
        let app = Application::new(config(Some("test-key")));
        let frozen = app.resolve_turn_model(None, None, ModelRole::Coder).await;
        app.begin_configuration_change()
            .unwrap()
            .replace_configuration(config(None), crate::api::ReloadSelection::Default)
            .await;
        assert!(app.resolve_turn_provider(&frozen).await.is_ok());
        let live = app.resolve_turn_model(None, None, ModelRole::Coder).await;
        match app.resolve_turn_provider(&live).await {
            Err(error) => assert_eq!(error.to_string(), "OpenRouter API key is not configured"),
            Ok(_) => panic!("missing credentials must prevent provider construction"),
        }
    }
}
