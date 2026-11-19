//! Exclusive access to configuration mutations for one application session.

use crate::api::{
    Application, ConfigurationChange, ConfigurationError, ConfigurationFileError,
    ConfigurationFileResult, ConfigurationPatch, ConfigurationSnapshot, OperationGuard,
    OperationInProgress, ReloadSelection,
};
use std::path::Path;
#[cfg(any(test, feature = "test-support"))]
use tauqe_core::config::AppConfig;
use tauqe_core::models::ModelSelection;

/// Holds the operation registration across parameter validation, file I/O,
/// configuration commit, and the adapter's response and event publication.
/// Dropping this object releases admission on every exit path.
pub struct ConfigurationOperation {
    application: Application,
    _guard: OperationGuard,
}

impl Application {
    pub(crate) fn begin_configuration_change(
        &self,
    ) -> Result<ConfigurationOperation, OperationInProgress> {
        let guard = self.try_begin_operation()?;
        Ok(ConfigurationOperation {
            application: self.clone(),
            _guard: guard,
        })
    }
}

#[cfg(feature = "test-support")]
impl Application {
    pub async fn test_replace_configuration(
        &self,
        config: AppConfig,
        selection: ReloadSelection,
    ) -> Result<(), OperationInProgress> {
        let _operation = self.begin_configuration_change()?;
        self.replace_configuration(config, selection).await;
        Ok(())
    }
}

impl ConfigurationOperation {
    pub(crate) async fn initialize_repository(
        &self,
        initial_commit: bool,
    ) -> Result<crate::api::RepositoryState, crate::api::RepositoryError> {
        crate::application::repository::initialize_repository(
            self.application.repository_root().await,
            initial_commit,
        )
        .await
    }

    /// Rebinds repository-owned state under the operation's exclusive admission.
    /// Usage publication remains synchronous and must not re-enter accounting.
    pub(crate) async fn rebind_repository<F>(&self, root: &Path, publish_usage: F)
    where
        F: Fn(&str, &crate::api::UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        if root.as_os_str().is_empty() {
            return;
        }
        let config = self.application.load_configuration(Some(root)).await;
        self.application
            .replace_configuration(config, ReloadSelection::Default)
            .await;
        if let Err(error) = self.application.rebind_context(root.to_path_buf()).await {
            tracing::warn!(%error, "Failed to rebind the context to the initialized repository");
        }
        self.application.reload_usage_lifetime(publish_usage).await;
        if let Err(error) = self.application.rebind_history(root.to_path_buf()).await {
            tracing::warn!(%error, "Failed to rebuild the interaction history for the repository");
        }
    }
    #[cfg(test)]
    pub(crate) async fn rebind_history(
        &self,
        root: std::path::PathBuf,
    ) -> Result<(), crate::api::HistoryError> {
        self.application.rebind_history(root).await
    }

    pub(crate) async fn select_model(
        &self,
        selection: ModelSelection,
    ) -> Result<ConfigurationChange, ConfigurationError> {
        self.application.select_model(selection).await
    }
    pub(crate) async fn update_project_configuration(
        &self,
        root: Option<&Path>,
        patch: ConfigurationPatch,
    ) -> Result<ConfigurationChange, ConfigurationError> {
        self.application
            .update_project_configuration(root, patch)
            .await
    }
    #[cfg(test)]
    pub(crate) async fn replace_configuration(
        &self,
        config: AppConfig,
        selection: ReloadSelection,
    ) -> ConfigurationChange {
        self.application
            .replace_configuration(config, selection)
            .await
    }
    pub(crate) async fn reload_configuration(&self, root: Option<&Path>) -> ConfigurationSnapshot {
        self.application.reload_configuration(root).await
    }
    pub(crate) async fn create_configuration(
        &self,
        root: Option<&Path>,
    ) -> Result<ConfigurationFileResult, ConfigurationFileError> {
        self.application.create_configuration(root).await
    }
    pub(crate) async fn save_credentials(
        &self,
        root: Option<&Path>,
        key: String,
    ) -> Result<ConfigurationFileResult, ConfigurationFileError> {
        self.application.save_credentials(root, key).await
    }
    pub(crate) async fn create_credentials_stub(
        &self,
        root: Option<&Path>,
    ) -> Result<ConfigurationFileResult, ConfigurationFileError> {
        self.application.create_credentials_stub(root).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::models::ModelRef;

    #[test]
    fn configuration_access_excludes_turns_and_other_writers_until_drop() {
        let app = Application::default();
        let clone = app.clone();
        let operation = app.begin_configuration_change().unwrap();
        assert!(clone.try_begin_operation().is_err());
        assert!(clone.begin_configuration_change().is_err());
        assert!(Application::default().begin_configuration_change().is_ok());
        drop(operation);
        assert!(clone.try_begin_operation().is_ok());
    }

    #[test]
    fn an_active_turn_refuses_configuration_access() {
        let app = Application::default();
        let turn = app.try_begin_operation().unwrap();
        assert!(app.begin_configuration_change().is_err());
        drop(turn);
        assert!(app.begin_configuration_change().is_ok());
    }

    #[tokio::test]
    async fn a_failed_mutation_retains_exclusivity_until_the_scenario_ends() {
        let app = Application::default();
        let operation = app.begin_configuration_change().unwrap();
        let result = operation.save_credentials(None, " \n ".into()).await;
        assert!(matches!(result, Err(ConfigurationFileError::InvalidKey)));
        assert!(app.try_begin_operation().is_err());
        drop(operation);
        assert!(app.try_begin_operation().is_ok());
    }

    #[tokio::test]
    async fn mutation_access_is_bound_to_its_original_session() {
        let mut config = AppConfig::default();
        config.models.repl = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];
        let app = Application::new(config.clone());
        let other = Application::new(config);
        let before = other.configuration().await.selection;
        let operation = app.begin_configuration_change().unwrap();
        let selection = ModelSelection::Specific(ModelRef::openrouter("beta/model"));
        operation.select_model(selection.clone()).await.unwrap();
        assert_eq!(app.configuration().await.selection, selection);
        assert_eq!(other.configuration().await.selection, before);
        assert!(app.try_begin_operation().is_err());
    }
}
