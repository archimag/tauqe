//! Configuration and status commands and their resolved views.
use crate::api::{
    Application, ApplicationEvent, ApplicationObserver, ConfigurationError, ConfigurationFileError,
    ConfigurationPatch, ConfigurationSnapshot, OperationInProgress, SystemStatus,
};
use std::{path::PathBuf, sync::Arc};
use tauqe_core::config::AppConfig;
use tauqe_core::models::{ModelRef, ModelRolesSummary, ModelSelection};
use tauqe_core::workflow::{WorkflowFactory, WorkflowKind};

#[derive(Clone)]
pub struct ConfigurationView {
    pub selection: ModelSelection,
    pub model: ModelRef,
    pub roles: ModelRolesSummary,
    pub history_model: ModelRef,
    pub workflow: WorkflowKind,
    pub available_workflows: Vec<WorkflowKind>,
    pub available_models: Vec<ModelRef>,
    pub review_presets: Vec<ReviewPresetView>,
    pub config_error: Option<String>,
}

#[derive(Clone)]
pub struct ReviewPresetView {
    pub name: String,
    pub title: Option<String>,
}

impl ConfigurationView {
    fn resolve(config: &AppConfig, selection: ModelSelection) -> Self {
        Self {
            model: config.resolve_model(&selection),
            selection,
            roles: config.effective_roles_summary(),
            history_model: config.history_model(),
            workflow: config.repl.workflow,
            available_workflows: WorkflowFactory::available_workflows(),
            available_models: config.available_models(),
            review_presets: config
                .review
                .presets
                .iter()
                .map(|preset| ReviewPresetView {
                    name: preset.name.clone(),
                    title: preset.title.clone(),
                })
                .collect(),
            config_error: config.config_error.clone(),
        }
    }
}

impl ConfigurationSnapshot {
    pub fn view(&self) -> ConfigurationView {
        ConfigurationView::resolve(&self.config, self.selection.clone())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("Another operation is already running")]
    Busy,
    #[error(transparent)]
    Configuration(#[from] ConfigurationError),
    #[error(transparent)]
    File(#[from] ConfigurationFileError),
}

pub enum ConfigurationCommand {
    Reload,
    Create,
    SaveCredentials(String),
    CreateCredentialsStub,
}

impl Application {
    async fn configuration_root(&self) -> Option<PathBuf> {
        let root = self.repository_root().await;
        (!root.as_os_str().is_empty()).then_some(root)
    }

    pub async fn configuration_view(&self) -> ConfigurationView {
        self.configuration().await.view()
    }

    pub async fn status(&self) -> SystemStatus {
        self.system_status(self.configuration_root().await.as_deref())
            .await
    }

    pub async fn change_model(
        &self,
        selection: ModelSelection,
        observer: &dyn ApplicationObserver,
    ) -> Result<ConfigurationView, SettingsError> {
        let operation = self
            .begin_configuration_change()
            .map_err(|_| SettingsError::Busy)?;
        let view = operation.select_model(selection).await?.snapshot.view();
        observer.publish(
            "",
            ApplicationEvent::ModelChanged {
                selection: view.selection.clone(),
                model: view.model.clone(),
                roles: Box::new(view.roles.clone()),
            },
        );
        Ok(view)
    }

    pub async fn configure(
        &self,
        patch: ConfigurationPatch,
        observer: &dyn ApplicationObserver,
    ) -> Result<ConfigurationView, SettingsError> {
        let operation = self
            .begin_configuration_change()
            .map_err(|_| SettingsError::Busy)?;
        let root = self.configuration_root().await;
        let view = operation
            .update_project_configuration(root.as_deref(), patch)
            .await?
            .snapshot
            .view();
        observer.publish(
            "",
            ApplicationEvent::ConfigurationChanged(Box::new(view.clone())),
        );
        Ok(view)
    }

    pub async fn change_configuration_files(
        &self,
        command: ConfigurationCommand,
        observer: &dyn ApplicationObserver,
    ) -> Result<SystemStatus, SettingsError> {
        let operation = self
            .begin_configuration_change()
            .map_err(|_| SettingsError::Busy)?;
        let root = self.configuration_root().await;
        let root = root.as_deref();
        let written = match command {
            ConfigurationCommand::Reload => {
                let snapshot = operation.reload_configuration(root).await;
                observer.publish(
                    "",
                    ApplicationEvent::ConfigurationChanged(Box::new(snapshot.view())),
                );
                return Ok(self.system_status(root).await);
            }
            ConfigurationCommand::Create => operation.create_configuration(root).await?,
            ConfigurationCommand::SaveCredentials(key) => {
                operation.save_credentials(root, key).await?
            }
            ConfigurationCommand::CreateCredentialsStub => {
                let written = operation.create_credentials_stub(root).await?;
                return Ok(self.system_status_after_write(root, &written).await);
            }
        };
        observer.publish(
            "",
            ApplicationEvent::ConfigurationChanged(Box::new(written.snapshot.view())),
        );
        Ok(self.system_status_after_write(root, &written).await)
    }

    pub async fn initialize_client(
        &self,
        observer: Arc<dyn ApplicationObserver>,
    ) -> Result<ClientInitialization, InitializationError> {
        let operation = self.begin_configuration_change()?;
        let repository = self.repository_state().await?;
        operation
            .rebind_repository(std::path::Path::new(&repository.root), observer)
            .await;
        Ok(ClientInitialization {
            repository,
            configuration: self.configuration_view().await,
            available_edit_protocols: tauqe_core::edits::EditProtocolFactory::available_protocols(),
        })
    }

    pub async fn initialize_repository(
        &self,
        initial_commit: bool,
        observer: Arc<dyn ApplicationObserver>,
    ) -> Result<crate::api::RepositoryState, InitializationError> {
        let operation = self.begin_configuration_change()?;
        let repository = operation.initialize_repository(initial_commit).await?;
        operation
            .rebind_repository(std::path::Path::new(&repository.root), observer.clone())
            .await;
        observer.publish("", ApplicationEvent::Repository(repository.clone()));
        Ok(repository)
    }
}

pub struct ClientInitialization {
    pub repository: crate::api::RepositoryState,
    pub configuration: ConfigurationView,
    pub available_edit_protocols: Vec<tauqe_core::workflow::EditProtocolKind>,
}

#[derive(Debug, thiserror::Error)]
pub enum InitializationError {
    #[error(transparent)]
    Busy(#[from] OperationInProgress),
    #[error(transparent)]
    Repository(#[from] crate::api::RepositoryError),
}
