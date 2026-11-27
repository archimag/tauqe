//! Read-only audit preview preparation and group aggregation.

use super::{find_preset, preset_name, AuditResources};
use crate::application::configuration::Configuration;
use crate::application::context::ContextSession;
use std::path::Path;
use tauqe_core::{
    config::AppConfig,
    review::window::{expand_window, ReviewWindow},
};
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
impl AuditResources {
    pub(crate) async fn preview(
        &self,
        configuration: &Configuration,
        preset: Option<&str>,
        paths: &[String],
    ) -> Result<ReviewPreview, ReviewPreviewError> {
        let config = configuration.snapshot().await.config;
        match preset_name(preset) {
            Some(name) => {
                let groups = configured_groups(&config, name)
                    .ok_or_else(|| ReviewPreviewError::PresetNotFound(name.into()))?;
                let root = self.context.repository_root().await;
                preview_preset(&root, &groups).await
            }
            None => preview_single(&self.context, paths).await,
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
    let preset = find_preset(config, name)?;
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
    context: &ContextSession,
    path_patterns: &[String],
) -> Result<ReviewPreview, String> {
    let context_snapshot = context.context_for_turn().await;
    let repo_root = context_snapshot.repo_root().to_path_buf();
    let expanded = expand_window(&repo_root, Some(context_snapshot), path_patterns)
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
/// per-group windows: the confirmation describes the whole session.
async fn preview_preset(
    repo_root: &Path,
    groups: &[ConfiguredGroup],
) -> Result<ReviewPreview, String> {
    let mut aggregated = ReviewWindow::default();
    let mut projected = Vec::with_capacity(groups.len());
    for group in groups {
        // A preset launch reads no interactive context, so every group expands
        // from an empty base exactly as the grouped audit does.
        let expanded = expand_window(repo_root, None, &group.paths)
            .await
            .map_err(|e| e.to_string())?;
        aggregated.accumulate(&expanded.window);
        projected.push(ReviewPreviewGroup {
            title: group.title.clone(),
            window: expanded.window,
        });
    }
    // Expansion keeps every readable match. Request admission, not preview,
    // decides whether the full input fits the shared model-request limit.
    Ok(ReviewPreview {
        window: aggregated,
        groups: projected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use tauqe_core::context::ContextAccess;

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

    #[tokio::test]
    async fn preset_preview_counts_every_group_without_a_file_allowance() {
        let root = tempfile::tempdir().unwrap();
        tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
        for path in ["a.rs", "b.rs"] {
            std::fs::write(root.path().join(path), "fn sample() {}\n").unwrap();
        }
        tauqe_core::git::cmd::stage_files(root.path(), &["a.rs".into(), "b.rs".into()]).unwrap();
        let mut config: AppConfig = toml::from_str(
            "[[review.presets]]\nname = 'audit'\n[[review.presets.groups]]\nname = 'first'\npaths = ['a.rs']\n[[review.presets.groups]]\nname = 'second'\npaths = ['b.rs']\n",
        ).unwrap();
        config.request.max_input_tokens = 1;
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(config, root.path().into(), sender);
        let preview = app.preview_review(Some("audit"), &[]).await.unwrap();
        assert_eq!(preview.groups.len(), 2);
        assert_eq!(preview.window.selected_files, 2);
        assert_eq!(preview.window.omitted_by_budget, 0);
        assert_eq!(preview.window.files_token_budget, 0);
        assert!(preview
            .groups
            .iter()
            .all(|group| group.window.selected_files == 1));
        assert!(app.active_operation_id().is_none());
    }
}
