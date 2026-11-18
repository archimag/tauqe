//! Atomic configuration and active model selection state.

use crate::config::AppConfig;
use crate::models::{ModelRef, ModelSelection};
use crate::workflow::WorkflowKind;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct ConfigurationSnapshot {
    pub config: AppConfig,
    pub selection: ModelSelection,
}

#[derive(Default)]
pub struct ConfigurationPatch {
    pub selection: Option<ModelSelection>,
    pub model: Option<ModelRef>,
    pub history_model: Option<ModelRef>,
    pub workflow: Option<WorkflowKind>,
}

#[derive(Clone, Copy)]
pub enum ReloadSelection {
    Default,
    Preserve,
}

pub struct ConfigurationChange {
    pub snapshot: ConfigurationSnapshot,
    pub previous_model: ModelRef,
}

#[derive(Debug)]
pub enum ConfigurationError {
    ModuleRead(String),
    ModuleConflict {
        key: &'static str,
        module: std::path::PathBuf,
    },

    UnknownModel {
        model: ModelRef,
        available: Vec<ModelRef>,
    },
    UnknownHistoryModel {
        model: ModelRef,
        available: Vec<ModelRef>,
    },
}

impl std::fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (label, model, available) = match self {
            Self::ModuleRead(reason) => return f.write_str(reason),
            Self::ModuleConflict { key, module } => {
                return write!(
                    f,
                    "The key `{}` is declared by the module {}; edit that file instead",
                    key,
                    module.display()
                )
            }
            Self::UnknownModel { model, available } => ("model", model, available),
            Self::UnknownHistoryModel { model, available } => ("history model", model, available),
        };
        write!(
            f,
            "Unknown {label} '{model}'. Available models: {}",
            available
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
impl std::error::Error for ConfigurationError {}

pub(crate) struct Configuration {
    state: Mutex<ConfigurationSnapshot>,
}
impl Default for Configuration {
    fn default() -> Self {
        Self::new(AppConfig::default())
    }
}
impl Configuration {
    pub(crate) fn new(config: AppConfig) -> Self {
        let selection = config.default_selection();
        Self {
            state: Mutex::new(ConfigurationSnapshot { config, selection }),
        }
    }
    pub(crate) async fn snapshot(&self) -> ConfigurationSnapshot {
        self.state.lock().await.clone()
    }

    pub(crate) async fn replace(
        &self,
        config: AppConfig,
        selection: ReloadSelection,
    ) -> ConfigurationChange {
        let mut state = self.state.lock().await;
        let previous_model = state.config.resolve_model(&state.selection);
        if matches!(selection, ReloadSelection::Default) {
            state.selection = config.default_selection();
        }
        state.config = config;
        ConfigurationChange {
            snapshot: state.clone(),
            previous_model,
        }
    }

    pub(crate) async fn update(
        &self,
        patch: ConfigurationPatch,
    ) -> Result<ConfigurationChange, ConfigurationError> {
        let mut state = self.state.lock().await;
        let previous_model = state.config.resolve_model(&state.selection);
        let available = state.config.available_models();
        let mut selection = state.selection.clone();
        if let Some(requested) = patch.selection {
            let model = state.config.resolve_model(&requested);
            if !available.contains(&model) {
                return Err(ConfigurationError::UnknownModel { model, available });
            }
            selection = requested;
        } else if let Some(model) = patch.model {
            if !available.contains(&model) {
                return Err(ConfigurationError::UnknownModel { model, available });
            }
            selection = ModelSelection::Specific(model);
        }
        let scribe = match patch.history_model {
            Some(model) => {
                if !available.contains(&model) {
                    return Err(ConfigurationError::UnknownHistoryModel { model, available });
                }
                Some(model)
            }
            None => state.config.models.scribe.clone(),
        };
        state.selection = selection;
        state.config.models.scribe = scribe;
        if let Some(workflow) = patch.workflow {
            state.config.repl.workflow = workflow;
        }
        Ok(ConfigurationChange {
            snapshot: state.clone(),
            previous_model,
        })
    }
}

impl crate::api::Application {
    /// Explicit CLI paths captured during bootstrap; core never parses argv.
    pub async fn explicit_configuration_paths(
        &self,
    ) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
        let snapshot = self.configuration().await;
        (
            snapshot.config.explicit_config_path,
            snapshot.config.explicit_credentials_path,
        )
    }

    /// Load without replacing state, so file creation and repository rebinding
    /// can preserve their existing commit and notification order.
    pub async fn load_configuration(&self, repo_root: Option<&std::path::Path>) -> AppConfig {
        let (config, credentials) = self.explicit_configuration_paths().await;
        crate::infrastructure::configuration::load(
            repo_root.map(std::path::Path::to_path_buf),
            config,
            credentials,
        )
        .await
    }

    /// Refuse module-owned fields before validating or committing any values.
    pub(crate) async fn update_project_configuration(
        &self,
        root: Option<&std::path::Path>,
        patch: ConfigurationPatch,
    ) -> Result<ConfigurationChange, ConfigurationError> {
        let mut keys = Vec::new();
        if patch.history_model.is_some() {
            keys.push("models.scribe");
        }
        if patch.workflow.is_some() {
            keys.push("repl.workflow");
        }
        if !keys.is_empty() {
            if let Some(root) = root {
                let owners = crate::infrastructure::configuration::owners(root.to_path_buf())
                    .await
                    .map_err(ConfigurationError::ModuleRead)?;
                if let Some((key, module)) = keys
                    .into_iter()
                    .find_map(|key| owners.get(key).map(|module| (key, module)))
                {
                    return Err(ConfigurationError::ModuleConflict {
                        key,
                        module: module.clone(),
                    });
                }
            }
        }
        self.update_configuration(patch).await
    }
}

#[cfg(test)]
mod file_scenario_tests {
    use super::*;
    use crate::api::Application;

    fn config() -> AppConfig {
        let mut config = AppConfig::default();
        config.models.repl = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];
        config
    }

    #[tokio::test]
    async fn module_ownership_precedes_validation_and_preserves_all_state() {
        let root = tempfile::tempdir().unwrap();
        let modules = root.path().join("tauqe.d");
        std::fs::create_dir(&modules).unwrap();
        let module = modules.join("models.toml");
        std::fs::write(&module, "[models]\nscribe = 'alpha/model'\n").unwrap();
        let app = Application::new(config());
        let before = app.configuration().await;
        let warm = app.session_uuid().await;
        let error = match app
            .update_project_configuration(
                Some(root.path()),
                ConfigurationPatch {
                    selection: Some(ModelSelection::Specific(ModelRef::openrouter("beta/model"))),
                    history_model: Some(ModelRef::openrouter("missing/model")),
                    workflow: Some(WorkflowKind::Naive),
                    ..Default::default()
                },
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("a module-owned change must be refused"),
        };
        assert!(
            matches!(error, ConfigurationError::ModuleConflict { key: "models.scribe", module: ref path } if path == &module)
        );
        let after = app.configuration().await;
        assert_eq!(after.selection, before.selection);
        assert_eq!(after.config.models.scribe, before.config.models.scribe);
        assert_eq!(after.config.repl.workflow, before.config.repl.workflow);
        assert_eq!(app.session_uuid().await, warm);
    }

    #[tokio::test]
    async fn runtime_selection_does_not_inspect_module_files() {
        let root = tempfile::tempdir().unwrap();
        let modules = root.path().join("tauqe.d");
        std::fs::create_dir(&modules).unwrap();
        std::fs::write(modules.join("broken.toml"), "not valid toml [").unwrap();
        let app = Application::new(config());
        let selection = ModelSelection::Specific(ModelRef::openrouter("beta/model"));
        app.update_project_configuration(
            Some(root.path()),
            ConfigurationPatch {
                selection: Some(selection.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(app.configuration().await.selection, selection);
    }

    #[tokio::test]
    async fn loading_preserves_cli_overrides_without_committing_state() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("explicit.toml");
        let credentials = root.path().join("credentials.toml");
        std::fs::write(&path, "[models]\nrepl = ['beta/model']\n").unwrap();
        let mut initial = config();
        initial.explicit_config_path = Some(path.clone());
        initial.explicit_credentials_path = Some(credentials.clone());
        let app = Application::new(initial);
        let before = app.configuration().await;
        let loaded = app.load_configuration(Some(root.path())).await;
        assert_eq!(loaded.explicit_config_path, Some(path));
        assert_eq!(loaded.explicit_credentials_path, Some(credentials));
        assert_eq!(
            loaded.resolve_model(&loaded.default_selection()),
            ModelRef::openrouter("beta/model")
        );
        let after = app.configuration().await;
        assert_eq!(after.selection, before.selection);
        assert_eq!(
            after.config.resolve_model(&after.selection),
            before.config.resolve_model(&before.selection)
        );
    }
}
