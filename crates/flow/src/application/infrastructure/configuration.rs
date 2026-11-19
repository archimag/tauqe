//! Blocking configuration loading, installation, and credential persistence.

use crate::api::ConfigurationFileError;
use std::path::PathBuf;
use tauqe_core::config::{
    load_config_with_options, module_owners, AppConfig, ConfigLoadOptions, ModuleOwners,
};

fn fallback_config_with_cli_paths(
    config: Option<PathBuf>,
    credentials: Option<PathBuf>,
) -> AppConfig {
    AppConfig {
        explicit_config_path: config,
        explicit_credentials_path: credentials,
        ..AppConfig::default()
    }
}

pub(crate) async fn load(
    repo_root: Option<PathBuf>,
    config: Option<PathBuf>,
    credentials: Option<PathBuf>,
) -> AppConfig {
    let fallback_config = config.clone();
    let fallback_credentials = credentials.clone();
    match tauqe_core::blocking::run_blocking_async(move || {
        load_config_with_options(ConfigLoadOptions {
            repo_root: repo_root.as_deref(),
            explicit_config: config.as_deref(),
            explicit_credentials: credentials.as_deref(),
        })
    })
    .await
    {
        Ok(config) => config,
        Err(error) => {
            tauqe_core::blocking::log_join_error(&error, "reload configuration");
            tracing::warn!(error = %error, "Failed to reload configuration; keeping defaults with CLI overrides preserved");
            fallback_config_with_cli_paths(fallback_config, fallback_credentials)
        }
    }
}

pub(crate) async fn owners(root: PathBuf) -> Result<ModuleOwners, String> {
    tauqe_core::blocking::run_blocking_async(move || module_owners(&root))
        .await
        .map_err(|error| {
            tauqe_core::blocking::log_join_error(&error, "inspect configuration module owners");
            format!("blocking task failed: {error}")
        })?
}

pub(crate) async fn create(
    root: Option<PathBuf>,
    explicit: Option<PathBuf>,
) -> Result<PathBuf, ConfigurationFileError> {
    tauqe_core::blocking::run_blocking_async(move || {
        let path = tauqe_core::config::default_config_path(root.as_deref(), explicit.as_deref());
        tauqe_core::config::ensure_no_module_conflict(
            tauqe_core::config::DEFAULT_CONFIG_TEMPLATE,
            &path,
            root.as_deref(),
        )
        .map_err(ConfigurationFileError::ModuleConflict)?;
        tauqe_core::config::write_default_config(&path).map_err(|error| {
            ConfigurationFileError::Failed(format!(
                "Failed to create config file at {:?}: {}",
                path, error
            ))
        })?;
        Ok(path)
    })
    .await
    .map_err(|error| {
        tauqe_core::blocking::log_join_error(&error, "create configuration");
        ConfigurationFileError::Failed(format!("blocking task failed: {error}"))
    })?
}

pub(crate) enum CredentialWrite {
    Key(String),
    Stub,
}

pub(crate) async fn write_credentials(
    root: Option<PathBuf>,
    explicit: Option<PathBuf>,
    write: CredentialWrite,
) -> Result<PathBuf, ConfigurationFileError> {
    tauqe_core::blocking::run_blocking_async(move || {
        let path =
            tauqe_core::config::default_credentials_path(root.as_deref(), explicit.as_deref());
        let (outcome, label) = match write {
            CredentialWrite::Key(key) => (
                tauqe_core::config::write_credentials_file(&path, &key),
                "file",
            ),
            CredentialWrite::Stub => (tauqe_core::config::write_credentials_stub(&path), "stub"),
        };
        outcome.map_err(|error| {
            ConfigurationFileError::Failed(format!(
                "Failed to write credentials {label} at {:?}: {}",
                path, error
            ))
        })?;
        Ok(path)
    })
    .await
    .map_err(|error| {
        tauqe_core::blocking::log_join_error(&error, "write credentials");
        ConfigurationFileError::Failed(format!("blocking task failed: {error}"))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_config_preserves_explicit_cli_paths() {
        let config = PathBuf::from("/tmp/tauqe/custom.toml");
        let credentials = PathBuf::from("/tmp/tauqe/credentials.toml");
        let fallback =
            fallback_config_with_cli_paths(Some(config.clone()), Some(credentials.clone()));
        assert_eq!(fallback.explicit_config_path, Some(config));
        assert_eq!(fallback.explicit_credentials_path, Some(credentials));
    }
}
