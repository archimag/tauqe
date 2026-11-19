//! Filesystem and Git probes for system status.

use crate::api::ConfigurationLocations;
use std::path::PathBuf;

pub(crate) async fn has_git(root: Option<PathBuf>) -> bool {
    tauqe_core::blocking::run_blocking_async(move || {
        tauqe_core::git::is_git_repository(root.as_deref())
    })
    .await
    .unwrap_or_else(|error| {
        tauqe_core::blocking::log_join_error(&error, "inspect system Git status");
        false
    })
}

pub(crate) async fn locations(
    root: Option<PathBuf>,
    config: Option<PathBuf>,
    credentials: Option<PathBuf>,
) -> ConfigurationLocations {
    tauqe_core::blocking::run_blocking_async(move || ConfigurationLocations {
        config: tauqe_core::config::find_config_file(root.as_deref(), config.as_deref()),
        credentials: tauqe_core::config::find_credentials_file(
            root.as_deref(),
            credentials.as_deref(),
        ),
        default_config: tauqe_core::config::default_config_path(root.as_deref(), config.as_deref()),
        default_credentials: tauqe_core::config::default_credentials_path(
            root.as_deref(),
            credentials.as_deref(),
        ),
    })
    .await
    .unwrap_or_else(|error| {
        tauqe_core::blocking::log_join_error(&error, "resolve system status paths");
        tracing::warn!(error = %error, "Failed to resolve the status paths");
        ConfigurationLocations::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovered_and_default_locations_honor_explicit_paths() {
        let root = tempfile::tempdir().unwrap();
        let repo_config = root.path().join("tauqe.toml");
        std::fs::write(&repo_config, "").unwrap();
        let paths = locations(Some(root.path().to_path_buf()), None, None).await;
        assert_eq!(paths.config, Some(repo_config.clone()));
        assert_eq!(paths.default_config, repo_config);
        let explicit = root.path().join("custom.toml");
        std::fs::write(&explicit, "").unwrap();
        let paths = locations(
            Some(root.path().to_path_buf()),
            Some(explicit.clone()),
            None,
        )
        .await;
        assert_eq!(paths.config, Some(explicit.clone()));
        assert_eq!(paths.default_config, explicit);
    }
}
