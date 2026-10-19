use std::path::{Path, PathBuf};

use super::paths::{dirs_config, dirs_home};
use super::providers;
use super::AppConfig;

/// Default path for the credentials file following the deterministic resolution order.
pub fn default_credentials_path(
    repo_root: Option<&Path>,
    explicit_credentials: Option<&Path>,
) -> PathBuf {
    default_credentials_path_internal(
        repo_root,
        dirs_home().as_deref(),
        dirs_config().as_deref(),
        explicit_credentials,
    )
}

pub(crate) fn default_credentials_path_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    explicit_credentials: Option<&Path>,
) -> PathBuf {
    if let Some(cli_creds) = explicit_credentials {
        if !cli_creds.as_os_str().is_empty() {
            return cli_creds.to_path_buf();
        }
    }
    if let Some(existing) =
        providers::find_credentials_file_internal(repo_root, home, config_dir, explicit_credentials)
    {
        return existing;
    }
    providers::credentials_candidates_internal(repo_root, home, config_dir, explicit_credentials)
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(".tauqe").join("credentials.toml"))
}

/// Loads credentials from an explicit path or the standard discovery chain,
/// applies the API key to the config, and falls back to the environment variable.
pub(crate) fn apply_credentials(
    config: &mut AppConfig,
    explicit_credentials: Option<&Path>,
    repo_root: Option<&Path>,
) {
    let creds =
        if let Some(explicit_path) = explicit_credentials.filter(|p| !p.as_os_str().is_empty()) {
            match std::fs::read_to_string(explicit_path) {
                Ok(content) => match toml::from_str::<providers::Credentials>(&content) {
                    Ok(creds) => Some(creds),
                    Err(err) => {
                        tracing::warn!(
                            "Failed to parse credentials file {}: {}",
                            explicit_path.display(),
                            err
                        );
                        let msg = format!(
                            "Failed to parse credentials file {}: {}",
                            explicit_path.display(),
                            err
                        );
                        config.config_error = match config.config_error.take() {
                            Some(prev) => Some(format!("{}; {}", prev, msg)),
                            None => Some(msg),
                        };
                        None
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        "Failed to read credentials file {}: {}",
                        explicit_path.display(),
                        err
                    );
                    let msg = format!(
                        "Failed to read credentials file {}: {}",
                        explicit_path.display(),
                        err
                    );
                    config.config_error = match config.config_error.take() {
                        Some(prev) => Some(format!("{}; {}", prev, msg)),
                        None => Some(msg),
                    };
                    None
                }
            }
        } else {
            providers::load_credentials(repo_root, None)
        };

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

    if explicit_credentials.is_none()
        && config
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_credentials_path_explicit_override() {
        let explicit = Path::new("/custom/my-credentials.toml");
        let path = default_credentials_path_internal(None, None, None, Some(explicit));
        assert_eq!(path, explicit);
    }

    #[test]
    fn test_default_credentials_path_targets_existing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let creds_file = temp_dir.path().join("existing-creds.toml");
        std::fs::write(&creds_file, "[openrouter]\napi_key = \"test\"\n").unwrap();

        let path = default_credentials_path_internal(None, None, None, Some(&creds_file));
        assert_eq!(path, creds_file);
    }

    #[test]
    fn test_default_credentials_path_unified_with_candidates_when_none_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = default_credentials_path_internal(Some(temp_dir.path()), None, None, None);
        assert_eq!(
            path,
            temp_dir.path().join(".tauqe").join("credentials.toml")
        );
    }

    #[test]
    fn test_apply_credentials_explicit_nonexistent_records_diagnostic_and_avoids_silent_fallback() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fallback_creds = temp_dir.path().join(".tauqe").join("credentials.toml");
        std::fs::create_dir_all(fallback_creds.parent().unwrap()).unwrap();
        std::fs::write(&fallback_creds, "[openrouter]\napi_key = \"sk-fallback\"\n").unwrap();

        let missing = temp_dir.path().join("missing.toml");
        let mut config = AppConfig::default();

        apply_credentials(&mut config, Some(&missing), Some(temp_dir.path()));

        assert!(config.config_error.is_some());
        assert!(config
            .config_error
            .unwrap()
            .contains("Failed to read credentials file"));
        assert!(config.providers.openrouter.is_none());
    }

    #[test]
    fn test_apply_credentials_explicit_malformed_records_diagnostic_and_avoids_silent_fallback() {
        let temp_dir = tempfile::tempdir().unwrap();
        let malformed = temp_dir.path().join("broken.toml");
        std::fs::write(&malformed, "not valid toml = = =").unwrap();

        let mut config = AppConfig::default();
        apply_credentials(&mut config, Some(&malformed), None);

        assert!(config.config_error.is_some());
        assert!(config
            .config_error
            .unwrap()
            .contains("Failed to parse credentials file"));
        assert!(config.providers.openrouter.is_none());
    }
}
