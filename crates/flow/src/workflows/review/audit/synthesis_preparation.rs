//! Freeze reviewer configuration and prepare the single-audit file window.

use super::AuditResources;
use tauqe_core::config::AppConfig;

use std::path::PathBuf;
use tauqe_core::context::ContextFileContent;
use tauqe_core::models::{ModelRef, ModelRole, ModelSelection};
use tauqe_core::review::window::expand_window;

pub struct ResolvedReviewSynthesis {
    #[cfg(test)]
    config: AppConfig,
    pub model: ModelRef,
}

pub struct PreparedReviewSynthesisWindow {
    pub root: PathBuf,
    pub files: Vec<ContextFileContent>,
    pub estimated_tokens: usize,
    pub omitted_by_budget: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewSynthesisPreparationError {
    #[error("Failed to prepare the audit window: {0}")]
    Read(String),
    #[error("Review path patterns matched no repository files: {0}")]
    NoPatternMatches(String),
    #[error("Context is empty: add files to review first")]
    EmptyContext,
}

impl ResolvedReviewSynthesis {
    pub(super) fn resolve(
        config: AppConfig,
        selection: Option<&ModelSelection>,
    ) -> ResolvedReviewSynthesis {
        let selection = selection
            .cloned()
            .unwrap_or(ModelSelection::Role(ModelRole::Reviewer));
        let model = config.resolve_model(&selection);
        ResolvedReviewSynthesis {
            #[cfg(test)]
            config,
            model,
        }
    }
}

impl AuditResources {
    pub(super) async fn prepare_review_synthesis_window(
        &self,
        _resolved: &ResolvedReviewSynthesis,
        path_patterns: &[String],
    ) -> Result<PreparedReviewSynthesisWindow, ReviewSynthesisPreparationError> {
        let ctx_snapshot = self.context.context_for_turn().await;
        let repo_root = ctx_snapshot.repo_root().to_path_buf();

        // The audit window is the union of the active context and every declared
        // line: a repository file is selected as soon as one pattern matches it, so
        // several paths widen the audit instead of competing for it. Blank lines
        // narrow nothing and are dropped before compilation (c-1, c-6).
        let patterns: Vec<String> = path_patterns
            .iter()
            .map(|pattern| pattern.trim())
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_string)
            .collect();

        let expanded = expand_window(&repo_root, Some(ctx_snapshot), &patterns)
            .await
            .map_err(|error| ReviewSynthesisPreparationError::Read(error.to_string()))?;
        if !patterns.is_empty() {
            // The expansion is the single deterministic projection of the audit
            // window: the preset launch and the read-only preview read the same
            // function, so the window the developer previewed is the window this
            // launch reads (#c-1, #c-4).
            // A pattern that selects nothing is refused rather than auditing
            // only the already loaded context.
            if expanded.selected_nothing() {
                return Err(ReviewSynthesisPreparationError::NoPatternMatches(
                    patterns.join(", "),
                ));
            }
        }
        if expanded.files.is_empty() {
            return Err(ReviewSynthesisPreparationError::EmptyContext);
        }

        Ok(PreparedReviewSynthesisWindow {
            root: repo_root,
            files: expanded.files,
            estimated_tokens: expanded.estimated_tokens,
            omitted_by_budget: 0,
        })
    }
}

#[cfg(test)]
impl crate::api::Application {
    pub(crate) async fn resolve_review_synthesis(
        &self,
        selection: Option<&ModelSelection>,
    ) -> ResolvedReviewSynthesis {
        ResolvedReviewSynthesis::resolve(self.configuration().await.config, selection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Application, ReloadSelection};
    use crate::application::configuration::ConfigurationSnapshot;
    use crate::application::ExecutionEnvironment;
    use tauqe_core::config::{ModelsConfig, OpenRouterConfig};
    use tauqe_core::context::ContextAccess;
    use tauqe_core::models::{ModelEntry, ReasoningEffort};

    fn config() -> AppConfig {
        AppConfig {
            models: ModelsConfig {
                reviewer: Some(vec![
                    ModelEntry::new(
                        ModelRef::openrouter("test/reviewer"),
                        Some(ReasoningEffort::Low),
                    ),
                    ModelEntry::new(
                        ModelRef::openrouter("test/reviewer"),
                        Some(ReasoningEffort::High),
                    ),
                ]),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn application(root: &std::path::Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(config(), root.to_path_buf(), sender)
    }
    fn track(root: &std::path::Path, files: &[(&str, &str)]) {
        tauqe_tools::git::state::init_repository(
            Some(root),
            true,
            tauqe_tools::git::GitHooks::Bypass,
        )
        .unwrap();
        for (path, content) in files {
            std::fs::write(root.join(path), content).unwrap();
        }
        tauqe_tools::git::cmd::run_git(Some(root), &["add", "-A"]).unwrap();
    }
    async fn add_context(app: &Application, path: &str) {
        let operation = app.begin_context_change().unwrap();
        operation
            .add_file(path.into(), ContextAccess::ReadOnly, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reviewer_resolution_preserves_explicit_model_for_repeated_models() {
        let app = Application::new(config());
        let base = app.resolve_review_synthesis(None).await;
        assert_eq!(base.model, ModelRef::openrouter("test/reviewer"));
        let selected = ModelSelection::Tier {
            model: base.model.clone(),
            effort: Some(ReasoningEffort::High),
        };
        let deep = app.resolve_review_synthesis(Some(&selected)).await;
        assert_eq!(deep.model, base.model);
    }

    #[tokio::test]
    async fn file_window_is_the_read_only_union_of_context_and_patterns() {
        let root = tempfile::tempdir().unwrap();
        track(root.path(), &[("held.rs", "held"), ("added.rs", "added")]);
        let app = application(root.path());
        add_context(&app, "held.rs").await;
        let before = app.context_state().await;
        let resolved = app.resolve_review_synthesis(None).await;
        let window = app
            .audit_resources()
            .prepare_review_synthesis_window(
                &resolved,
                &[
                    " held.rs ".into(),
                    "added.rs".into(),
                    "added.rs".into(),
                    " ".into(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            window
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["held.rs", "added.rs"]
        );
        assert_eq!(window.omitted_by_budget, 0);
        assert!(window.estimated_tokens > before.total_estimated_tokens as usize);
        assert_eq!(app.context_state().await, before);
        assert!(app.active_operation_id().is_none());
    }

    #[tokio::test]
    async fn no_matches_are_refused_even_when_context_exists_and_blank_patterns_keep_it() {
        let root = tempfile::tempdir().unwrap();
        track(root.path(), &[("held.rs", "held")]);
        let app = application(root.path());
        let resolved = app.resolve_review_synthesis(None).await;
        assert!(matches!(
            app.audit_resources()
                .prepare_review_synthesis_window(&resolved, &[])
                .await,
            Err(ReviewSynthesisPreparationError::EmptyContext)
        ));
        add_context(&app, "held.rs").await;
        assert!(
            matches!(app.audit_resources().prepare_review_synthesis_window(&resolved, &["missing/**".into()]).await,
            Err(ReviewSynthesisPreparationError::NoPatternMatches(patterns)) if patterns == "missing/**")
        );
        let window = app
            .audit_resources()
            .prepare_review_synthesis_window(&resolved, &[" ".into()])
            .await
            .unwrap();
        assert_eq!(window.files.len(), 1);
        assert_eq!(window.files[0].path, "held.rs");
    }

    #[tokio::test]
    async fn frozen_credentials_and_budget_survive_configuration_changes() {
        let root = tempfile::tempdir().unwrap();
        track(root.path(), &[("audit.rs", "audit")]);
        let app = application(root.path());
        let frozen = app.resolve_review_synthesis(None).await;
        let mut changed = config();
        changed.request.max_input_tokens = 1;
        changed.providers.openrouter = Some(OpenRouterConfig {
            api_key: Some("test-key".into()),
            ..Default::default()
        });
        app.replace_configuration(changed, ReloadSelection::Preserve)
            .await;
        let frozen_environment = ExecutionEnvironment::new(ConfigurationSnapshot {
            selection: frozen.config.default_selection(),
            config: frozen.config.clone(),
        });
        assert!(frozen_environment
            .provider(&app, frozen.model.provider)
            .await
            .is_err());
        let window = app
            .audit_resources()
            .prepare_review_synthesis_window(&frozen, &["audit.rs".into()])
            .await
            .unwrap();
        assert_eq!(window.files.len(), 1);
        let current = app.resolve_review_synthesis(None).await;
        let current_environment = ExecutionEnvironment::new(ConfigurationSnapshot {
            selection: current.config.default_selection(),
            config: current.config.clone(),
        });
        assert!(current_environment
            .provider(&app, current.model.provider)
            .await
            .is_ok());
        assert_eq!(
            app.audit_resources()
                .prepare_review_synthesis_window(&current, &["audit.rs".into()])
                .await
                .unwrap()
                .files
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn full_window_keeps_context_and_every_match() {
        let root = tempfile::tempdir().unwrap();
        track(
            root.path(),
            &[("held.rs", "held"), ("a.rs", "a"), ("b.rs", "b")],
        );
        let app = application(root.path());
        add_context(&app, "held.rs").await;
        let spent = app.context_state().await.total_estimated_tokens as usize;
        let mut changed = config();
        changed.request.max_input_tokens = 1;
        app.replace_configuration(changed, ReloadSelection::Preserve)
            .await;
        let resolved = app.resolve_review_synthesis(None).await;
        let window = app
            .audit_resources()
            .prepare_review_synthesis_window(&resolved, &["a.rs".into(), "b.rs".into()])
            .await
            .unwrap();
        assert_eq!(
            window
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["held.rs", "a.rs", "b.rs"]
        );
        assert!(window.estimated_tokens > spent);
        assert_eq!(window.omitted_by_budget, 0);
    }
}
