//! Freeze reviewer configuration and prepare the single-audit file window.

use crate::api::Application;
use tauqe_core::config::AppConfig;

use std::path::PathBuf;
use tauqe_core::context::ContextFileContent;
use tauqe_core::models::{ModelRef, ModelRole, ModelSelection, ReasoningEffort};
use tauqe_core::review::window::{expand_window, WindowBudgets, WindowContext};

pub struct ResolvedReviewSynthesis {
    pub(crate) config: AppConfig,
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
}

pub struct PreparedReviewSynthesisWindow {
    pub root: PathBuf,
    pub files: Vec<ContextFileContent>,
    pub estimated_tokens: usize,
    pub omitted_by_budget: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewSynthesisPreparationError {
    #[error("Review path patterns matched no repository files: {0}")]
    NoPatternMatches(String),
    #[error("Context is empty: add files to review first")]
    EmptyContext,
}

impl Application {
    /// Resolve before provider setup and before reading the active context.
    pub(crate) async fn resolve_review_synthesis(
        &self,
        selection: Option<&ModelSelection>,
    ) -> ResolvedReviewSynthesis {
        let config = self.configuration().await.config;
        let selection = selection
            .cloned()
            .unwrap_or(ModelSelection::Role(ModelRole::Reviewer));
        let model = config.resolve_model(&selection);
        let effort = config
            .models
            .resolve_selection_effort(ModelRole::Reviewer, &selection);
        ResolvedReviewSynthesis {
            config,
            model,
            effort,
        }
    }

    /// Prepare before claiming the operation slot, preserving refusal priority.
    pub(crate) async fn prepare_review_synthesis_window(
        &self,
        resolved: &ResolvedReviewSynthesis,
        path_patterns: &[String],
    ) -> Result<PreparedReviewSynthesisWindow, ReviewSynthesisPreparationError> {
        let budgets = WindowBudgets {
            files_tokens: resolved.config.context.token_budget,
        };
        let (repo_root, ctx_snapshot, mut estimated_tokens) = {
            let ctx = self.context_for_turn().await;
            (
                ctx.repo_root().to_path_buf(),
                ctx.clone(),
                ctx.get_state().total_estimated_tokens as usize,
            )
        };

        let mut files = match tokio::task::spawn_blocking(move || ctx_snapshot.read_context_files())
            .await
        {
            Ok(files) => files,
            Err(err) => {
                tracing::warn!(error = %format!("blocking task failed: {err}"), "Failed to read context files in offload");
                Vec::new()
            }
        };

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

        // Pattern-matched files the configured budgets kept out of the audit.
        // Reported on the started event so a truncated expansion is never silent.
        let mut omitted_files = 0usize;

        if !patterns.is_empty() {
            // The expansion is the single deterministic projection of the audit
            // window: the preset launch and the read-only preview read the same
            // function, so the window the developer previewed is the window this
            // launch reads (#c-1, #c-4).
            let expanded = expand_window(
                &repo_root,
                WindowContext {
                    files: &files,
                    tokens: estimated_tokens,
                },
                &patterns,
                budgets,
            )
            .await;

            match expanded {
                Ok(expanded) => {
                    // A pattern that selects nothing is a mistyped or unsupported
                    // path, never a narrower audit: launching anyway would review a
                    // different file set while the developer believes the pattern
                    // chose it. A partial selection stays a lawful run reported
                    // through `omitted_files`, so only a zero match is refused.
                    if expanded.selected_nothing() {
                        return Err(ReviewSynthesisPreparationError::NoPatternMatches(
                            patterns.join(", "),
                        ));
                    }
                    estimated_tokens = expanded.estimated_tokens;
                    omitted_files = expanded.window.omitted_by_budget;
                    if omitted_files > 0 {
                        tracing::warn!(
                        omitted = omitted_files,
                        selected = expanded.window.selected_files,
                        matched = expanded.window.matched_files,
                        "path pattern expansion exhausted the configured file token budget; remaining matched files were not loaded"
                    );
                    }
                    files.extend(expanded.files);
                }
                Err(err) => {
                    // The listing failed at the infrastructure level, which says
                    // nothing about the pattern itself: the audit proceeds with the
                    // context files already loaded instead of rejecting a pattern the
                    // harness never evaluated.
                    tracing::warn!(
                        error = %err,
                        "Failed to list repository files for the review path pattern"
                    );
                }
            }
        }

        if files.is_empty() {
            return Err(ReviewSynthesisPreparationError::EmptyContext);
        }

        Ok(PreparedReviewSynthesisWindow {
            root: repo_root,
            files,
            estimated_tokens,
            omitted_by_budget: omitted_files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ReloadSelection;
    use tauqe_core::config::{ModelsConfig, OpenRouterConfig};
    use tauqe_core::context::ContextAccess;
    use tauqe_core::models::ModelEntry;

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
    async fn reviewer_resolution_preserves_explicit_effort_for_repeated_models() {
        let app = Application::new(config());
        let base = app.resolve_review_synthesis(None).await;
        assert_eq!(base.model, ModelRef::openrouter("test/reviewer"));
        assert_eq!(base.effort, Some(ReasoningEffort::Low));
        let selected = ModelSelection::Tier {
            model: base.model.clone(),
            effort: Some(ReasoningEffort::High),
        };
        let deep = app.resolve_review_synthesis(Some(&selected)).await;
        assert_eq!(deep.model, base.model);
        assert_eq!(deep.effort, Some(ReasoningEffort::High));
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
            app.prepare_review_synthesis_window(&resolved, &[]).await,
            Err(ReviewSynthesisPreparationError::EmptyContext)
        ));
        add_context(&app, "held.rs").await;
        assert!(
            matches!(app.prepare_review_synthesis_window(&resolved, &["missing/**".into()]).await,
            Err(ReviewSynthesisPreparationError::NoPatternMatches(patterns)) if patterns == "missing/**")
        );
        let window = app
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
        changed.context.token_budget = 0;
        changed.providers.openrouter = Some(OpenRouterConfig {
            api_key: Some("test-key".into()),
            ..Default::default()
        });
        app.replace_configuration(changed, ReloadSelection::Preserve)
            .await;
        assert!(app
            .resolve_provider(frozen.model.provider, &frozen.config)
            .await
            .is_err());
        let window = app
            .prepare_review_synthesis_window(&frozen, &["audit.rs".into()])
            .await
            .unwrap();
        assert_eq!(window.files.len(), 1);
        let current = app.resolve_review_synthesis(None).await;
        assert!(app
            .resolve_provider(current.model.provider, &current.config)
            .await
            .is_ok());
        assert!(matches!(
            app.prepare_review_synthesis_window(&current, &["audit.rs".into()])
                .await,
            Err(ReviewSynthesisPreparationError::EmptyContext)
        ));
    }

    #[tokio::test]
    async fn exhausted_context_budget_reports_omissions_without_dropping_the_context() {
        let root = tempfile::tempdir().unwrap();
        track(
            root.path(),
            &[("held.rs", "held"), ("a.rs", "a"), ("b.rs", "b")],
        );
        let app = application(root.path());
        add_context(&app, "held.rs").await;
        let spent = app.context_state().await.total_estimated_tokens as usize;
        let mut changed = config();
        changed.context.token_budget = spent;
        app.replace_configuration(changed, ReloadSelection::Preserve)
            .await;
        let resolved = app.resolve_review_synthesis(None).await;
        let window = app
            .prepare_review_synthesis_window(&resolved, &["a.rs".into(), "b.rs".into()])
            .await
            .unwrap();
        assert_eq!(window.files.len(), 1);
        assert_eq!(window.files[0].path, "held.rs");
        assert_eq!(window.estimated_tokens, spent);
        assert_eq!(window.omitted_by_budget, 2);
    }
}
