//! Model resolution, provider construction, and runtime policy of a turn.

use crate::api::Application;
use crate::config::AppConfig;
use crate::models::{ModelRef, ModelRole, ModelSelection, ReasoningEffort};
use crate::providers::{format_session_id, LlmProvider, SESSION_PREFIX_REPL};
use crate::workflow::policy::{ApplyPolicy, DirectApplyPolicy};
use crate::workflow::WorkflowKind;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone, Copy)]
pub enum TurnMode {
    Interactive,
    Step,
}

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

impl ResolvedTurnModel {
    pub fn provider(&self) -> Result<Arc<dyn LlmProvider>, TurnProviderError> {
        crate::providers::create_provider(self.model.provider, &self.config)
            .map(Arc::from)
            .map_err(|error| TurnProviderError(error.to_string()))
    }
}

impl Application {
    pub async fn resolve_turn_model(
        &self,
        selection: Option<ModelSelection>,
        effort: Option<ReasoningEffort>,
        mode: TurnMode,
    ) -> ResolvedTurnModel {
        let snapshot = self.configuration().await;
        let config = snapshot.config;
        let selection = selection.unwrap_or(snapshot.selection);
        let model = config.resolve_model(&selection);
        let role = match &selection {
            ModelSelection::Role(role) => Some(*role),
            ModelSelection::Specific(_) | ModelSelection::Tier { .. } => None,
        };
        let effort_role = match mode {
            TurnMode::Interactive => role.unwrap_or(ModelRole::REPL),
            TurnMode::Step => ModelRole::Coder,
        };
        let effort = effort.or_else(|| config.resolve_selection_effort(effort_role, &selection));
        let alias = config.model_alias(&model).map(str::to_string);
        ResolvedTurnModel {
            config,
            selection,
            model,
            role,
            effort,
            alias,
        }
    }

    pub async fn turn_session(&self, mode: TurnMode, explicit: Option<String>) -> Option<String> {
        if explicit.is_some() {
            return explicit;
        }
        match mode {
            TurnMode::Interactive => Some(format_session_id(
                SESSION_PREFIX_REPL,
                &self.session_uuid().await,
            )),
            TurnMode::Step => None,
        }
    }

    pub(crate) fn apply_policy(
        &self,
        workflow: WorkflowKind,
        cancel: watch::Receiver<bool>,
    ) -> Box<dyn ApplyPolicy> {
        match workflow {
            WorkflowKind::Naive => Box::new(DirectApplyPolicy::new()),
            WorkflowKind::Git => Box::new(self.git_transaction_policy(cancel)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelsConfig, OpenRouterConfig, ProvidersConfig};
    use crate::models::{ModelCatalogEntry, ModelEntry};

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
            .resolve_turn_model(None, None, TurnMode::Interactive)
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
            .resolve_turn_model(Some(selection.clone()), None, TurnMode::Interactive)
            .await;
        let step = app
            .resolve_turn_model(Some(selection), None, TurnMode::Step)
            .await;
        assert_eq!(interactive.role, None);
        assert_eq!(step.role, None);
        assert_eq!(interactive.effort, Some(ReasoningEffort::Low));
        assert_eq!(step.effort, Some(ReasoningEffort::High));
    }

    #[tokio::test]
    async fn explicit_effort_precedes_tier_and_role_effort() {
        let app = Application::new(config(None));
        for mode in [TurnMode::Interactive, TurnMode::Step] {
            let selection = ModelSelection::Tier {
                model: ModelRef::openrouter("shared/model"),
                effort: Some(ReasoningEffort::Max),
            };
            let tier = app
                .resolve_turn_model(Some(selection.clone()), None, mode)
                .await;
            assert_eq!(tier.role, None);
            assert_eq!(tier.effort, Some(ReasoningEffort::Max));
            let explicit = app
                .resolve_turn_model(Some(selection), Some(ReasoningEffort::Medium), mode)
                .await;
            assert_eq!(explicit.effort, Some(ReasoningEffort::Medium));
        }
    }

    #[tokio::test]
    async fn provider_construction_keeps_the_credentials_of_the_resolved_snapshot() {
        let app = Application::new(config(Some("test-key")));
        let frozen = app.resolve_turn_model(None, None, TurnMode::Step).await;
        app.begin_configuration_change()
            .unwrap()
            .replace_configuration(config(None), crate::api::ReloadSelection::Default)
            .await;
        assert!(frozen.provider().is_ok());
        let live = app.resolve_turn_model(None, None, TurnMode::Step).await;
        match live.provider() {
            Err(error) => assert_eq!(error.to_string(), "OpenRouter API key is not configured"),
            Ok(_) => panic!("missing credentials must prevent provider construction"),
        }
    }

    #[tokio::test]
    async fn interactive_affinity_and_explicit_session_precedence_are_preserved() {
        let app = Application::new(config(None));
        let identity = app.session_uuid().await;
        assert_eq!(
            app.turn_session(TurnMode::Interactive, None).await,
            Some(format!("tauqe-repl-{identity}"))
        );
        assert_eq!(app.turn_session(TurnMode::Step, None).await, None);
        for mode in [TurnMode::Interactive, TurnMode::Step] {
            assert_eq!(
                app.turn_session(mode, Some("explicit-session".into()))
                    .await,
                Some("explicit-session".into())
            );
            assert_eq!(
                app.turn_session(mode, Some(String::new())).await,
                Some(String::new())
            );
        }
        assert_eq!(identity, app.session_uuid().await);
    }
}
