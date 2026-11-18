//! System readiness and file locations independent of the RPC representation.

use super::configuration_files::ConfigurationFileKind;
use crate::api::{Application, ConfigurationFileResult, ConfigurationSnapshot};
use std::path::{Path, PathBuf};

#[derive(Clone, Default)]
pub struct ConfigurationLocations {
    pub config: Option<PathBuf>,
    pub credentials: Option<PathBuf>,
    pub default_config: PathBuf,
    pub default_credentials: PathBuf,
}

pub struct SystemStatus {
    pub configuration: ConfigurationSnapshot,
    pub locations: ConfigurationLocations,
    pub has_git: bool,
    pub has_api_key: bool,
    pub ready: bool,
}

impl Application {
    pub async fn system_status(&self, root: Option<&Path>) -> SystemStatus {
        let has_git =
            crate::infrastructure::system_status::has_git(root.map(Path::to_path_buf)).await;
        let (config, credentials) = self.explicit_configuration_paths().await;
        let locations = crate::infrastructure::system_status::locations(
            root.map(Path::to_path_buf),
            config,
            credentials,
        )
        .await;
        let configuration = self.configuration().await;
        let has_api_key = crate::config::has_openrouter_key(&configuration.config);
        SystemStatus {
            configuration,
            locations,
            has_git,
            has_api_key,
            ready: has_git && has_api_key,
        }
    }

    /// Report the file this scenario actually wrote, even if a filesystem probe
    /// cannot rediscover it. Notification ordering remains with the RPC adapter.
    pub async fn system_status_after_write(
        &self,
        root: Option<&Path>,
        written: &ConfigurationFileResult,
    ) -> SystemStatus {
        let mut status = self.system_status(root).await;
        match written.kind {
            ConfigurationFileKind::Config => status.locations.config = Some(written.path.clone()),
            ConfigurationFileKind::Credentials => {
                status.locations.credentials = Some(written.path.clone())
            }
        }
        status.configuration.selection = written.snapshot.selection.clone();
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, OpenRouterConfig};

    fn config(root: &Path) -> AppConfig {
        AppConfig {
            explicit_config_path: Some(root.join("tauqe.toml")),
            explicit_credentials_path: Some(root.join("credentials.toml")),
            ..AppConfig::default()
        }
    }

    #[tokio::test]
    async fn readiness_requires_both_git_and_a_key_but_not_a_config_file() {
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config(root.path()));
        let empty = app.system_status(Some(root.path())).await;
        assert!(!empty.has_git && !empty.has_api_key && !empty.ready);
        crate::git::init_repository(Some(root.path()), false).unwrap();
        let git_only = app.system_status(Some(root.path())).await;
        assert!(git_only.has_git);
        assert!(!git_only.ready);
        let mut configured = config(root.path());
        configured.providers.openrouter = Some(OpenRouterConfig {
            api_key: Some("test-key".into()),
            ..Default::default()
        });
        let ready = Application::new(configured)
            .system_status(Some(root.path()))
            .await;
        assert!(ready.has_git && ready.has_api_key && ready.ready);
        assert!(ready.locations.config.is_none());
    }

    #[tokio::test]
    async fn post_write_status_preserves_the_written_file_even_if_not_rediscovered() {
        let root = tempfile::tempdir().unwrap();
        let app = Application::new(config(root.path()));
        let written = app.create_configuration(Some(root.path())).await.unwrap();
        std::fs::remove_file(&written.path).unwrap();
        let report = app
            .system_status_after_write(Some(root.path()), &written)
            .await;
        assert_eq!(report.locations.config.as_ref(), Some(&written.path));
        assert_eq!(report.configuration.selection, written.snapshot.selection);
        assert!(app
            .system_status(Some(root.path()))
            .await
            .locations
            .config
            .is_none());
        let written = app
            .create_credentials_stub(Some(root.path()))
            .await
            .unwrap();
        std::fs::remove_file(&written.path).unwrap();
        let report = app
            .system_status_after_write(Some(root.path()), &written)
            .await;
        assert_eq!(report.locations.credentials.as_ref(), Some(&written.path));
        assert!(app
            .system_status(Some(root.path()))
            .await
            .locations
            .credentials
            .is_none());
    }
}
