use crate::models::ModelCatalogEntry;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::paths::{dirs_config, dirs_home};
use super::AppConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub openrouter: Option<OpenRouterConfig>,
    #[serde(default)]
    pub openai: Option<OpenAiConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenRouterConfig {
    /// In-memory API key loaded exclusively from credentials or environment.
    /// Never deserialized from or serialized into tauqe.toml.
    #[serde(skip)]
    pub api_key: Option<String>,
    /// Provider catalog: the models this project exposes to the harness.
    ///
    /// This is the single source of truth for model identity and display
    /// aliases. An entry is either a bare model specification or a
    /// `{ model, alias }` table, so a model's display name is declared once,
    /// beside the model itself, instead of being repeated on every role slot
    /// that selects it.
    #[serde(default)]
    pub models: Vec<ModelCatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenAiConfig {
    #[serde(default, deserialize_with = "deserialize_openai_models")]
    models: Vec<ModelCatalogEntry>,
}

fn deserialize_openai_models<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ModelCatalogEntry>, D::Error> {
    struct Entry(ModelCatalogEntry);
    impl<'de> Deserialize<'de> for Entry {
        fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            ModelCatalogEntry::deserialize_in(de, Some(crate::models::ProviderKind::OpenAI))
                .map(Self)
        }
    }
    Ok(Vec::<Entry>::deserialize(deserializer)?
        .into_iter()
        .map(|entry| entry.0)
        .collect())
}

impl ProvidersConfig {
    pub(crate) fn catalog(&self) -> impl Iterator<Item = &ModelCatalogEntry> {
        self.openrouter
            .iter()
            .flat_map(|config| &config.models)
            .chain(self.openai.iter().flat_map(|config| &config.models))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Credentials {
    #[serde(default)]
    pub openrouter: Option<OpenRouterCredentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct OpenRouterCredentials {
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Returns whether a non-empty OpenRouter API key is available.
pub fn has_openrouter_key(config: &AppConfig) -> bool {
    config
        .providers
        .openrouter
        .as_ref()
        .and_then(|o| o.api_key.as_deref())
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
fn credentials_candidates(
    repo_root: Option<&Path>,
    explicit_credentials: Option<&Path>,
) -> Vec<PathBuf> {
    credentials_candidates_internal(
        repo_root,
        dirs_home().as_deref(),
        dirs_config().as_deref(),
        explicit_credentials,
    )
}

pub(crate) fn credentials_candidates_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    cli_credentials: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(explicit) = cli_credentials {
        if !explicit.as_os_str().is_empty() {
            candidates.push(explicit.to_path_buf());
            return candidates;
        }
    }

    let mut add_candidate = |p: PathBuf| {
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    };

    if let Some(root) = repo_root {
        add_candidate(root.join(".tauqe").join("credentials.toml"));
    } else if let Ok(cwd) = std::env::current_dir() {
        add_candidate(cwd.join(".tauqe").join("credentials.toml"));
    }

    if let Some(cfg) = config_dir {
        add_candidate(cfg.join("tauqe").join("credentials.toml"));
    }

    if let Some(h) = home {
        add_candidate(h.join(".config").join("tauqe").join("credentials.toml"));
        add_candidate(h.join(".tauqe").join("credentials.toml"));
    }

    candidates
}

pub fn find_credentials_file(
    repo_root: Option<&Path>,
    explicit_credentials: Option<&Path>,
) -> Option<PathBuf> {
    find_credentials_file_internal(
        repo_root,
        dirs_home().as_deref(),
        dirs_config().as_deref(),
        explicit_credentials,
    )
}

pub(crate) fn find_credentials_file_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    cli_credentials: Option<&Path>,
) -> Option<PathBuf> {
    credentials_candidates_internal(repo_root, home, config_dir, cli_credentials)
        .into_iter()
        .find(|p| p.is_file())
}

pub fn load_credentials(
    repo_root: Option<&Path>,
    explicit_credentials: Option<&Path>,
) -> Option<Credentials> {
    load_credentials_internal(
        repo_root,
        dirs_home().as_deref(),
        dirs_config().as_deref(),
        explicit_credentials,
    )
}

fn load_credentials_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    cli_credentials: Option<&Path>,
) -> Option<Credentials> {
    let path = find_credentials_file_internal(repo_root, home, config_dir, cli_credentials)?;
    read_credentials_file(&path)
}

pub fn read_credentials_file(path: &Path) -> Option<Credentials> {
    match std::fs::read_to_string(path) {
        Ok(content) => match toml::from_str::<Credentials>(&content) {
            Ok(creds) => Some(creds),
            Err(_) => {
                tracing::warn!("Failed to parse credentials file {:?}", path);
                None
            }
        },
        Err(err) => {
            tracing::warn!("Failed to read credentials file {:?}: {}", path, err);
            None
        }
    }
}

/// Serializes edits across daemon processes and preserves unrelated TOML keys.
#[cfg(unix)]
pub(crate) fn update_credentials<T, E: From<std::io::Error>>(
    path: &Path,
    edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<T, E>,
) -> Result<T, E> {
    use std::io::{Error, ErrorKind, Write};
    use std::os::unix::fs::OpenOptionsExt;

    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(error) if error.kind() == ErrorKind::NotFound => path.to_owned(),
        Err(error) => return Err(error.into()),
    };
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(target.with_added_extension("lock"))?;
    lock.lock()?;

    let mut document = match std::fs::read_to_string(&target) {
        Ok(text) => text
            .parse()
            .map_err(|_| Error::new(ErrorKind::InvalidData, "Invalid credentials TOML"))?,
        Err(error) if error.kind() == ErrorKind::NotFound => toml_edit::DocumentMut::new(),
        Err(error) => return Err(error.into()),
    };
    let result = edit(&mut document)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(document.to_string().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(target).map_err(std::io::Error::from)?;
    Ok(result)
}

#[cfg(not(unix))]
pub(crate) fn update_credentials<T, E: From<std::io::Error>>(
    _path: &Path,
    _edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<T, E>,
) -> Result<T, E> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Owner-only credential files require Unix",
    )
    .into())
}

fn update_openrouter_key(path: &Path, key: &str, overwrite: bool) -> std::io::Result<()> {
    update_credentials(path, |document| {
        let section = document
            .entry("openrouter")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let table = section.as_table_like_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Expected an openrouter credentials table",
            )
        })?;
        if overwrite || !table.contains_key("api_key") {
            table.insert("api_key", toml_edit::value(key));
        }
        Ok(())
    })
}

pub fn write_credentials_file(path: &Path, api_key: &str) -> std::io::Result<()> {
    update_openrouter_key(path, api_key.trim(), true)
}

pub fn write_credentials_stub(path: &Path) -> std::io::Result<()> {
    update_openrouter_key(path, "sk-or-v1-YOUR-KEY-HERE", false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn test_credentials_candidates_precedence() {
        let explicit = Path::new("/custom/creds.toml");
        let candidates = credentials_candidates(None, Some(explicit));
        assert_eq!(candidates, vec![explicit.to_path_buf()]);

        let temp_dir = tempfile::tempdir().unwrap();
        let candidates = credentials_candidates_internal(Some(temp_dir.path()), None, None, None);
        assert_eq!(
            candidates,
            vec![temp_dir.path().join(".tauqe").join("credentials.toml")]
        );
    }

    #[test]
    fn test_parse_credentials_toml() {
        let toml_str = r#"
[openrouter]
api_key = "sk-or-v1-secret-token"
"#;
        let creds: Credentials = toml::from_str(toml_str).expect("Failed to parse credentials");
        assert_eq!(
            creds.openrouter.as_ref().and_then(|o| o.api_key.as_deref()),
            Some("sk-or-v1-secret-token")
        );
    }

    #[test]
    fn test_load_credentials_from_project_tauqe_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let dot_wb = temp_dir.path().join(".tauqe");
        std::fs::create_dir_all(&dot_wb).unwrap();
        std::fs::write(
            dot_wb.join("credentials.toml"),
            "[openrouter]\napi_key = \"sk-proj-key\"\n",
        )
        .unwrap();

        let creds = load_credentials_internal(Some(temp_dir.path()), None, None, None);
        assert!(creds.is_some());
        assert_eq!(
            creds.unwrap().openrouter.unwrap().api_key.as_deref(),
            Some("sk-proj-key")
        );
    }

    #[test]
    fn test_load_credentials_from_user_config_dir() {
        let temp_home = tempfile::tempdir().unwrap();
        let config_wb = temp_home.path().join(".config").join("tauqe");
        std::fs::create_dir_all(&config_wb).unwrap();
        std::fs::write(
            config_wb.join("credentials.toml"),
            "[openrouter]\napi_key = \"sk-global-key\"\n",
        )
        .unwrap();

        let creds = load_credentials_internal(None, Some(temp_home.path()), None, None);
        assert!(creds.is_some());
        assert_eq!(
            creds.unwrap().openrouter.unwrap().api_key.as_deref(),
            Some("sk-global-key")
        );
    }

    #[test]
    fn test_project_credentials_take_precedence_over_global() {
        let temp_project = tempfile::tempdir().unwrap();
        let proj_wb = temp_project.path().join(".tauqe");
        std::fs::create_dir_all(&proj_wb).unwrap();
        std::fs::write(
            proj_wb.join("credentials.toml"),
            "[openrouter]\napi_key = \"sk-project-priority\"\n",
        )
        .unwrap();

        let temp_home = tempfile::tempdir().unwrap();
        let config_wb = temp_home.path().join(".config").join("tauqe");
        std::fs::create_dir_all(&config_wb).unwrap();
        std::fs::write(
            config_wb.join("credentials.toml"),
            "[openrouter]\napi_key = \"sk-global-ignored\"\n",
        )
        .unwrap();

        let creds = load_credentials_internal(
            Some(temp_project.path()),
            Some(temp_home.path()),
            None,
            None,
        );
        assert!(creds.is_some());
        assert_eq!(
            creds.unwrap().openrouter.unwrap().api_key.as_deref(),
            Some("sk-project-priority")
        );
    }

    #[test]
    fn openai_catalog_resolves_bare_names_and_aliases() {
        let config: AppConfig = toml::from_str(
            r#"
[providers.openrouter]
models = ["vendor/model"]
[providers.openai]
models = [{model="example", alias="Subscribed"}]
[models]
repl = ["Subscribed"]
"#,
        )
        .unwrap();
        assert_eq!(
            config
                .resolve_role(crate::models::ModelRole::REPL)
                .to_spec(),
            "openai:example"
        );
        assert_eq!(
            config.model_catalog().next().unwrap().model.to_spec(),
            "openrouter:vendor/model"
        );
        assert!(
            toml::from_str::<AppConfig>("[providers.openai]\nmodels=['openrouter:other']").is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_credentials_file_and_check() {
        let temp_dir = tempfile::tempdir().unwrap();
        let creds_path = temp_dir.path().join(".tauqe").join("credentials.toml");

        let mut cfg = AppConfig::default();
        assert!(!has_openrouter_key(&cfg));

        write_credentials_file(&creds_path, "sk-test-token-12345").unwrap();
        let creds = read_credentials_file(&creds_path).unwrap();
        assert_eq!(
            creds.openrouter.as_ref().and_then(|o| o.api_key.as_deref()),
            Some("sk-test-token-12345")
        );

        cfg.providers.openrouter = Some(OpenRouterConfig {
            api_key: Some("sk-test-token-12345".to_string()),
            ..Default::default()
        });
        assert!(has_openrouter_key(&cfg));
    }

    #[cfg(unix)]
    #[test]
    fn test_write_credentials_stub() {
        let temp_dir = tempfile::tempdir().unwrap();
        let stub_path = temp_dir.path().join("credentials.toml");
        write_credentials_stub(&stub_path).unwrap();

        let creds = read_credentials_file(&stub_path).unwrap();
        assert_eq!(
            creds.openrouter.as_ref().and_then(|o| o.api_key.as_deref()),
            Some("sk-or-v1-YOUR-KEY-HERE")
        );
    }

    #[cfg(unix)]
    #[test]
    fn updating_one_provider_preserves_other_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.toml");
        std::fs::write(&path, "[openai]\nclient_id = 'existing'\n").unwrap();
        write_credentials_file(&path, "updated").unwrap();
        write_credentials_stub(&path).unwrap();

        let doc: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(doc["openai"]["client_id"].as_str(), Some("existing"));
        assert_eq!(doc["openrouter"]["api_key"].as_str(), Some("updated"));
    }

    #[cfg(unix)]
    #[test]
    fn test_write_credentials_file_escapes_special_characters() {
        let temp_dir = tempfile::tempdir().unwrap();
        let creds_path = temp_dir.path().join("credentials.toml");
        let tricky_key = "sk-\"injected\"\napi_key = \"evil\"";

        write_credentials_file(&creds_path, tricky_key).unwrap();

        let creds = read_credentials_file(&creds_path).expect("credentials must round-trip");
        assert_eq!(
            creds.openrouter.as_ref().and_then(|o| o.api_key.as_deref()),
            Some(tricky_key)
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_credentials_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let creds_path = temp_dir.path().join("credentials.toml");
        write_credentials_file(&creds_path, "sk-secret").unwrap();

        let mode = std::fs::metadata(&creds_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The owner-only guarantee is the only supported way to persist a secret,
    /// so a target that cannot enforce it refuses the write instead of leaving
    /// the key readable by other accounts.
    #[cfg(not(unix))]
    #[test]
    fn test_write_credentials_file_is_refused_without_owner_only_permissions() {
        let temp_dir = tempfile::tempdir().unwrap();
        let creds_path = temp_dir.path().join(".tauqe").join("credentials.toml");

        let err = write_credentials_file(&creds_path, "sk-secret").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        assert!(
            !creds_path.exists(),
            "a refused write must not leave the secret on disk"
        );
    }

    #[test]
    fn test_tauqe_toml_cannot_set_api_key() {
        let toml_str = r#"
[providers.openrouter]
api_key = "insecure-key-in-repo"
models = ["deepseek/deepseek-chat"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(
            config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref()),
            None
        );
    }

    #[test]
    fn test_catalog_entries_accept_aliases() {
        let toml_str = r#"
[providers.openrouter]
models = [
    "google/gemini-flash-latest",
    { model = "deepseek/deepseek-chat", alias = "Deep" },
]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        let catalog: Vec<_> = config.providers.catalog().collect();

        assert_eq!(catalog.len(), 2);
        assert_eq!(
            catalog[0].model,
            crate::models::ModelRef::openrouter("google/gemini-flash-latest")
        );
        assert_eq!(catalog[0].alias, None);
        assert_eq!(
            catalog[1].model,
            crate::models::ModelRef::openrouter("deepseek/deepseek-chat")
        );
        assert_eq!(catalog[1].alias.as_deref(), Some("Deep"));
    }
}
