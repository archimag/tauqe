//! Repository queries, initialization, and squash-preview policy.
use crate::api::{Application, OperationGuard};
use crate::git::{state::RepositoryState, CommitSummary};
use tauqe_tools::git::diff::FileDiff;
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
}
pub struct SquashPreview {
    pub base_ref: String,
    pub session_base: Option<String>,
    pub upstream_base: Option<String>,
    pub commits: Vec<CommitSummary>,
    pub diff_stat: String,
    pub files: Vec<FileDiff>,
}
async fn offload<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RepositoryError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| RepositoryError::Failed(format!("blocking task failed: {error}")))
}
impl Application {
    pub async fn repository_state(&self) -> Result<RepositoryState, RepositoryError> {
        let root = self.repository_root().await;
        offload(move || crate::git::get_repository_state(Some(&root))).await
    }
    pub async fn repository_files(&self) -> Result<Vec<String>, RepositoryError> {
        let root = self.repository_root().await;
        offload(move || crate::git::list_repository_files(Some(&root)))
            .await?
            .map_err(|error| RepositoryError::Failed(error.to_string()))
    }
    pub async fn repository_diff(
        &self,
        commit: Option<String>,
        path: Option<String>,
    ) -> Result<String, RepositoryError> {
        let commit = commit
            .map(|revision| {
                crate::git::validate_revision(&revision)
                    .map_err(|error| RepositoryError::Invalid(error.into()))
            })
            .transpose()?;
        let root = self.repository_root().await;
        offload(move || match commit {
            Some(commit) => crate::git::get_commit_diff(&root, &commit),
            None => crate::git::get_diff(&root, path.as_deref()),
        })
        .await?
        .map_err(|error| RepositoryError::Failed(error.to_string()))
    }
    pub async fn squash_preview(
        &self,
        operation: &OperationGuard,
        base: Option<String>,
    ) -> Result<SquashPreview, RepositoryError> {
        if !self.owns_operation(operation) {
            return Err(RepositoryError::Failed(
                "squash preview requires an active operation for this session".into(),
            ));
        }
        let root = self.repository_root().await;
        let upstream = self.configuration().await.config.git.upstream.clone();
        let requested = base
            .map(|revision| {
                crate::git::validate_revision(&revision)
                    .map_err(|error| RepositoryError::Invalid(error.into()))
            })
            .transpose()?;
        offload(move || {
            let session_base = crate::git::find_last_non_ai_commit(&root).unwrap_or(None);
            let upstream_base = crate::git::detect_upstream_branch(&root, upstream.as_deref());
            let base_ref = requested
                .or_else(|| session_base.clone())
                .or_else(|| upstream_base.clone())
                .unwrap_or_else(|| "HEAD~1".into());
            Ok::<_, anyhow::Error>(SquashPreview {
                commits: crate::git::get_commits_ahead(&root, &base_ref)?,
                diff_stat: crate::git::get_diff_stat(&root, &base_ref)?,
                files: crate::git::get_cumulative_file_diffs(&root, &base_ref)?,
                base_ref,
                session_base,
                upstream_base,
            })
        })
        .await?
        .map_err(|error| RepositoryError::Failed(error.to_string()))
    }
}
pub(super) async fn initialize_repository(
    root: std::path::PathBuf,
    initial_commit: bool,
) -> Result<RepositoryState, RepositoryError> {
    offload(move || crate::git::init_repository(Some(&root), initial_commit))
        .await?
        .map_err(|error| RepositoryError::Failed(error.to_string()))
}
pub fn discover_repository() -> RepositoryState {
    crate::git::get_repository_state(None)
}
pub fn repository_configuration(
    root: &std::path::Path,
    config: Option<&std::path::Path>,
    credentials: Option<&std::path::Path>,
) -> crate::config::AppConfig {
    crate::config::load_config_with_options(crate::config::ConfigLoadOptions {
        repo_root: Some(root),
        explicit_config: config,
        explicit_credentials: credentials,
    })
}

#[cfg(test)]
mod tests;
