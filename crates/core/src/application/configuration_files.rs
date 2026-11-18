//! Configuration file scenarios and their in-memory commit policy.

use crate::api::{Application, ConfigurationSnapshot, ReloadSelection};
use crate::infrastructure::configuration::{self, CredentialWrite};
use std::path::{Path, PathBuf};

pub(crate) enum ConfigurationFileKind {
    Config,
    Credentials,
}

pub struct ConfigurationFileResult {
    pub(crate) kind: ConfigurationFileKind,
    pub path: PathBuf,
    pub snapshot: ConfigurationSnapshot,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigurationFileError {
    #[error("API key cannot be empty")]
    InvalidKey,
    #[error("{0}")]
    ModuleConflict(String),
    #[error("{0}")]
    Failed(String),
}

impl Application {
    /// Reload, reset active selection, and invalidate the dialogue only when
    /// the new effective model differs. RPC publication belongs to the adapter.
    pub(crate) async fn reload_configuration(&self, root: Option<&Path>) -> ConfigurationSnapshot {
        let loaded = self.load_configuration(root).await;
        let change = self
            .replace_configuration(loaded, ReloadSelection::Default)
            .await;
        let model = change
            .snapshot
            .config
            .resolve_model(&change.snapshot.selection);
        self.maybe_rotate_session_on_model_change(&change.previous_model, &model)
            .await;
        change.snapshot
    }

    pub(crate) async fn create_configuration(
        &self,
        root: Option<&Path>,
    ) -> Result<ConfigurationFileResult, ConfigurationFileError> {
        let (explicit, _) = self.explicit_configuration_paths().await;
        let path = configuration::create(root.map(Path::to_path_buf), explicit).await?;
        let snapshot = self.reload_configuration(root).await;
        Ok(ConfigurationFileResult {
            path,
            snapshot,
            kind: ConfigurationFileKind::Config,
        })
    }

    pub(crate) async fn save_credentials(
        &self,
        root: Option<&Path>,
        api_key: String,
    ) -> Result<ConfigurationFileResult, ConfigurationFileError> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(ConfigurationFileError::InvalidKey);
        }
        let (_, explicit) = self.explicit_configuration_paths().await;
        let path = configuration::write_credentials(
            root.map(Path::to_path_buf),
            explicit,
            CredentialWrite::Key(api_key.to_string()),
        )
        .await?;
        let loaded = self.load_configuration(root).await;
        let change = self
            .replace_configuration(loaded, ReloadSelection::Preserve)
            .await;
        Ok(ConfigurationFileResult {
            path,
            snapshot: change.snapshot,
            kind: ConfigurationFileKind::Credentials,
        })
    }

    pub(crate) async fn create_credentials_stub(
        &self,
        root: Option<&Path>,
    ) -> Result<ConfigurationFileResult, ConfigurationFileError> {
        let (_, explicit) = self.explicit_configuration_paths().await;
        let path = configuration::write_credentials(
            root.map(Path::to_path_buf),
            explicit,
            CredentialWrite::Stub,
        )
        .await?;
        // A stub is an installation aid, not a configuration reload.
        Ok(ConfigurationFileResult {
            path,
            snapshot: self.configuration().await,
            kind: ConfigurationFileKind::Credentials,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::models::{ModelRef, ModelSelection};

    fn application(root: &Path) -> Application {
        let mut config = AppConfig {
            explicit_config_path: Some(root.join("tauqe.toml")),
            explicit_credentials_path: Some(root.join("credentials.toml")),
            ..AppConfig::default()
        };
        config.models.repl = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];
        Application::new(config)
    }

    #[tokio::test]
    async fn a_conflicting_template_does_not_create_files_or_change_state() {
        let root = tempfile::tempdir().unwrap();
        let modules = root.path().join("tauqe.d");
        std::fs::create_dir(&modules).unwrap();
        std::fs::write(
            modules.join("models.toml"),
            "[models]\nrepl = ['beta/model']\n",
        )
        .unwrap();
        let app = application(root.path());
        let before = app.configuration().await;
        let session = app.session_uuid().await;
        assert!(matches!(
            app.create_configuration(Some(root.path())).await,
            Err(ConfigurationFileError::ModuleConflict(_))
        ));
        assert!(!root.path().join("tauqe.toml").exists());
        assert!(!root.path().join("Conventions.md").exists());
        assert_eq!(app.configuration().await.selection, before.selection);
        assert_eq!(app.session_uuid().await, session);
    }

    #[tokio::test]
    async fn saving_credentials_trims_the_key_and_preserves_selection_and_dialogue() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("tauqe.toml"),
            "[models]\nrepl = ['alpha/model', 'beta/model']\n",
        )
        .unwrap();
        let app = application(root.path());
        let selection = ModelSelection::Specific(ModelRef::openrouter("beta/model"));
        app.select_model(selection.clone()).await.unwrap();
        let session = app.session_uuid().await;
        let result = app
            .save_credentials(Some(root.path()), "  test-token\n".into())
            .await
            .unwrap();
        assert_eq!(result.path, root.path().join("credentials.toml"));
        assert_eq!(result.snapshot.selection, selection);
        assert_eq!(
            result
                .snapshot
                .config
                .providers
                .openrouter
                .as_ref()
                .and_then(|provider| provider.api_key.as_deref()),
            Some("test-token")
        );
        assert_eq!(app.session_uuid().await, session);
        assert_eq!(app.configuration().await.selection, selection);
    }

    #[tokio::test]
    async fn blank_credentials_are_rejected_before_writing() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let session = app.session_uuid().await;
        assert!(matches!(
            app.save_credentials(Some(root.path()), " \n ".into()).await,
            Err(ConfigurationFileError::InvalidKey)
        ));
        assert!(!root.path().join("credentials.toml").exists());
        assert_eq!(app.session_uuid().await, session);
    }

    #[tokio::test]
    async fn installing_a_stub_does_not_reload_configuration() {
        let root = tempfile::tempdir().unwrap();
        // A changed on-disk model would be observed if stub creation reloaded.
        std::fs::write(
            root.path().join("tauqe.toml"),
            "[models]\nrepl = ['new/model']\n",
        )
        .unwrap();
        let app = application(root.path());
        let before = app.configuration().await;
        let session = app.session_uuid().await;
        let result = app
            .create_credentials_stub(Some(root.path()))
            .await
            .unwrap();
        assert!(result.path.is_file());
        assert_eq!(
            result
                .snapshot
                .config
                .resolve_model(&result.snapshot.selection),
            before.config.resolve_model(&before.selection)
        );
        assert_eq!(app.session_uuid().await, session);
    }
}
