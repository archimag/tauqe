//! Read-only audit preview preparation and group aggregation.

use crate::{
    api::Application,
    config::AppConfig,
    review::window::{expand_window, ReviewWindow, WindowBudgets, WindowContext},
};
use std::path::Path;
struct ConfiguredGroup {
    title: String,
    paths: Vec<String>,
}
pub struct ReviewPreviewGroup {
    pub title: String,
    pub window: ReviewWindow,
}
pub struct ReviewPreview {
    pub window: ReviewWindow,
    pub groups: Vec<ReviewPreviewGroup>,
}
#[derive(Debug, thiserror::Error)]
pub enum ReviewPreviewError {
    #[error("Review preset '{0}' not found")]
    PresetNotFound(String),
    #[error("{0}")]
    Read(String),
}
impl Application {
    pub async fn preview_review(
        &self,
        preset: Option<&str>,
        paths: &[String],
    ) -> Result<ReviewPreview, ReviewPreviewError> {
        let config = self.configuration().await.config;
        let budgets = WindowBudgets {
            files_tokens: config.context.token_budget,
        };
        let root = self.repository_root().await;
        match preset.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => {
                let groups = configured_groups(&config, name)
                    .ok_or_else(|| ReviewPreviewError::PresetNotFound(name.into()))?;
                preview_preset(&root, &groups, budgets).await
            }
            None => preview_single(self, &root, paths, budgets).await,
        }
        .map_err(ReviewPreviewError::Read)
    }
}
/// Resolves a configured preset into the groups the projection measures.
///
/// The lookup matches the launch exactly — the trimmed requested name against
/// the trimmed configured name — so a preview never describes a preset the
/// launch would fail to resolve and never falls back to a preset the developer
/// did not name.
fn configured_groups(config: &AppConfig, name: &str) -> Option<Vec<ConfiguredGroup>> {
    let preset = config
        .review
        .presets
        .iter()
        .find(|preset| preset.name.trim() == name)?;
    Some(
        preset
            .groups
            .iter()
            .map(|group| ConfiguredGroup {
                title: group.name.clone(),
                paths: group.paths.clone(),
            })
            .collect(),
    )
}

/// Measures the window a plain launch reads: the active context and patterns.
async fn preview_single(
    application: &Application,
    repo_root: &Path,
    path_patterns: &[String],
    budgets: WindowBudgets,
) -> Result<ReviewPreview, String> {
    let (context_snapshot, context_tokens) = {
        let context = application.context_for_turn().await;
        (
            context.clone(),
            context.get_state().total_estimated_tokens as usize,
        )
    };
    // Reading the active context touches the file system, so it runs on the
    // blocking pool exactly as the launch reads it.
    let files = tokio::task::spawn_blocking(move || context_snapshot.read_context_files())
        .await
        .map_err(|err| format!("blocking task failed: {err}"))
        .map_err(|err| format!("Failed to read the active context: {}", err))?;

    let expanded = expand_window(
        repo_root,
        WindowContext {
            files: &files,
            tokens: context_tokens,
        },
        path_patterns,
        budgets,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(ReviewPreview {
        window: expanded.window,
        groups: Vec::new(),
    })
}

/// Measures every configured group and folds the windows into one projection.
///
/// Groups audit their own slices, so the aggregate sums the counters of the
/// per-group windows: the confirmation a client renders then describes the whole
/// session instead of one slice of it.
async fn preview_preset(
    repo_root: &Path,
    groups: &[ConfiguredGroup],
    budgets: WindowBudgets,
) -> Result<ReviewPreview, String> {
    let mut aggregated = ReviewWindow::default();
    let mut projected = Vec::with_capacity(groups.len());
    for group in groups {
        // A preset launch reads no interactive context, so every group expands
        // from an empty base exactly as the grouped audit does.
        let expanded = expand_window(repo_root, WindowContext::empty(), &group.paths, budgets)
            .await
            .map_err(|e| e.to_string())?;
        aggregated.accumulate(&expanded.window);
        projected.push(ReviewPreviewGroup {
            title: group.title.clone(),
            window: expanded.window,
        });
    }
    // Every group window obeys its own configured file budget, so the aggregate
    // allowance is what each group may spend; comparing a summed spend against a
    // single group's limit would misreport the whole session.
    Ok(ReviewPreview {
        window: aggregated,
        groups: projected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;

    #[tokio::test]
    async fn preview_reads_context_without_admission_or_mutation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("held.rs"), "fn held() {}\n").unwrap();
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), sender);
        {
            let context = app.begin_context_change().unwrap();
            context
                .add_file("held.rs".into(), ContextAccess::ReadOnly, None)
                .await
                .unwrap();
        }
        let before = app.context_state().await;
        let operation = app.try_begin_operation().unwrap();
        let result = app.preview_review(None, &[]).await.unwrap();
        assert_eq!(result.window.context_files, 1);
        assert!(result.groups.is_empty());
        assert_eq!(app.context_state().await, before);
        assert_eq!(app.active_operation_id(), Some(operation.id()));
        assert!(
            matches!(app.preview_review(Some("missing"), &[]).await, Err(ReviewPreviewError::PresetNotFound(name)) if name == "missing")
        );
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }
}
