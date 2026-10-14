use std::path::{Path, PathBuf};

use super::paths::{dirs_config, dirs_home};
use super::providers;
use super::AppConfig;

/// Default path for the credentials file following the deterministic resolution order.
pub fn default_credentials_path(
    repo_root: Option<&Path>,
    explicit_credentials: Option<&Path>,
) -> PathBuf {
    if let Some(cli_creds) = explicit_credentials {
        if !cli_creds.as_os_str().is_empty() {
            return cli_creds.to_path_buf();
        }
    }
    if let Some(cfg) = dirs_config() {
        return cfg.join("tauqe").join("credentials.toml");
    }
    if let Some(home) = dirs_home() {
        return home.join(".config").join("tauqe").join("credentials.toml");
    }
    if let Some(root) = repo_root {
        if !root.as_os_str().is_empty() {
            return root.join(".tauqe").join("credentials.toml");
        }
    }
    PathBuf::from(".tauqe").join("credentials.toml")
}

/// Loads credentials from an explicit path or the standard discovery chain,
/// applies the API key to the config, and falls back to the environment variable.
pub(crate) fn apply_credentials(
    config: &mut AppConfig,
    explicit_credentials: Option<&Path>,
    repo_root: Option<&Path>,
) {
    let creds = explicit_credentials
        .and_then(providers::read_credentials_file)
        .or_else(|| providers::load_credentials(repo_root, None));

    if let Some(creds) = creds {
        if let Some(or_creds) = creds.openrouter {
            if let Some(key) = or_creds.api_key {
                let trimmed = key.trim();
                if !trimmed.is_empty() {
                    let mut or_cfg = config.providers.openrouter.take().unwrap_or_default();
                    or_cfg.api_key = Some(trimmed.to_string());
                    config.providers.openrouter = Some(or_cfg);
                }
            }
        }
    }

    if config
        .providers
        .openrouter
        .as_ref()
        .and_then(|o| o.api_key.as_deref())
        .is_none()
    {
        if let Ok(env_key) = std::env::var("OPENROUTER_API_KEY") {
            let trimmed = env_key.trim();
            if !trimmed.is_empty() {
                let mut or_cfg = config.providers.openrouter.take().unwrap_or_default();
                or_cfg.api_key = Some(trimmed.to_string());
                config.providers.openrouter = Some(or_cfg);
            }
        }
    }
}
