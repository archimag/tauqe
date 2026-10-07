use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub models: ModelsConfig,
    #[serde(default)]
    pub edit: EditConfig,
    #[serde(default)]
    pub toolchain: ToolchainConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GitConfig {
    #[serde(default)]
    pub upstream: Option<String>,
}

pub const DEFAULT_MAX_DISCOVERY_ROUNDS: usize = 5;

fn default_max_discovery_rounds() -> usize {
    DEFAULT_MAX_DISCOVERY_ROUNDS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default, deserialize_with = "deserialize_pinned")]
    pub pinned: Vec<String>,
    #[serde(default = "default_max_discovery_rounds")]
    pub max_discovery_rounds: usize,
    #[serde(default)]
    pub max_auto_files_per_round: Option<usize>,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            max_discovery_rounds: DEFAULT_MAX_DISCOVERY_ROUNDS,
            max_auto_files_per_round: None,
        }
    }
}

fn deserialize_pinned<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum PinnedRaw {
        List(Vec<String>),
        Single(String),
        TableWithFiles { files: Vec<String> },
        TableKeys(std::collections::BTreeMap<String, toml::Value>),
    }

    let raw = PinnedRaw::deserialize(deserializer)?;
    let mut files = Vec::new();
    match raw {
        PinnedRaw::List(list) => {
            for s in list {
                let s = s.trim().to_string();
                if !s.is_empty() && !files.contains(&s) {
                    files.push(s);
                }
            }
        }
        PinnedRaw::Single(s) => {
            let s = s.trim().to_string();
            if !s.is_empty() {
                files.push(s);
            }
        }
        PinnedRaw::TableWithFiles { files: list } => {
            for s in list {
                let s = s.trim().to_string();
                if !s.is_empty() && !files.contains(&s) {
                    files.push(s);
                }
            }
        }
        PinnedRaw::TableKeys(map) => {
            for (k, v) in map {
                let path = match v {
                    toml::Value::String(s) if !s.trim().is_empty() => s.trim().to_string(),
                    _ => k.trim().to_string(),
                };
                if !path.is_empty() && !files.contains(&path) {
                    files.push(path);
                }
            }
        }
    }
    Ok(files)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolchainConfig {
    #[serde(default)]
    pub check_command: Option<String>,
    #[serde(default)]
    pub max_retries: Option<usize>,
    #[serde(default)]
    pub auto_heal: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub openrouter: Option<OpenRouterConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenRouterConfig {
    #[serde(default)]
    pub api_key: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    /// Active (default) model. Empty means "take first of `available`, or built-in default".
    #[serde(default)]
    pub default: String,
    /// Models the user may switch between. Always contains `default` after `normalize()`.
    #[serde(default)]
    pub available: Vec<String>,
}

pub const BUILTIN_DEFAULT_MODEL: &str = "anthropic/claude-3.5-sonnet";

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            default: BUILTIN_DEFAULT_MODEL.to_string(),
            available: vec![BUILTIN_DEFAULT_MODEL.to_string()],
        }
    }
}

impl ModelsConfig {
    /// Trims, de-duplicates and guarantees that `default` is non-empty and listed in `available`.
    pub fn normalize(&mut self) {
        let mut cleaned: Vec<String> = Vec::new();
        for m in self.available.drain(..) {
            let m = m.trim().to_string();
            if !m.is_empty() && !cleaned.contains(&m) {
                cleaned.push(m);
            }
        }

        let mut default = self.default.trim().to_string();
        if default.is_empty() {
            default = cleaned
                .first()
                .cloned()
                .unwrap_or_else(|| BUILTIN_DEFAULT_MODEL.to_string());
        }
        if !cleaned.contains(&default) {
            cleaned.insert(0, default.clone());
        }

        self.default = default;
        self.available = cleaned;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditConfig {
    #[serde(default = "default_workflow")]
    pub workflow: String, // "git", "naive"
    #[serde(default = "default_protocol")]
    pub protocol: String, // "xml", "structured"
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

fn default_workflow() -> String {
    "git".to_string()
}

fn default_protocol() -> String {
    "xml".to_string()
}

fn default_max_retries() -> usize {
    3
}

impl Default for EditConfig {
    fn default() -> Self {
        Self {
            workflow: default_workflow(),
            protocol: default_protocol(),
            max_retries: default_max_retries(),
        }
    }
}

pub fn config_candidates(repo_root: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(cfg_path) = std::env::var("TAUQE_CONFIG") {
        let trimmed = cfg_path.trim();
        if !trimmed.is_empty() {
            candidates.push(PathBuf::from(trimmed));
        }
    }

    let mut add_candidate = |p: PathBuf| {
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    };

    if let Some(root) = repo_root {
        add_candidate(root.join("tauqe.toml"));
        add_candidate(root.join(".tauqe.toml"));
    }

    if let Ok(cwd) = std::env::current_dir() {
        add_candidate(cwd.join("tauqe.toml"));
        add_candidate(cwd.join(".tauqe.toml"));
    }

    if let Some(cfg) = dirs_config() {
        add_candidate(cfg.join("tauqe").join("tauqe.toml"));
    }

    if let Some(home) = dirs_home() {
        add_candidate(home.join(".config").join("tauqe").join("tauqe.toml"));
        add_candidate(home.join(".tauqe.toml"));
    }

    candidates
}

pub fn find_config_file(repo_root: Option<&Path>) -> Option<PathBuf> {
    config_candidates(repo_root).into_iter().find(|p| p.is_file())
}

pub fn default_config_path(repo_root: Option<&Path>) -> PathBuf {
    if let Some(root) = repo_root {
        if !root.as_os_str().is_empty() {
            return root.join("tauqe.toml");
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd.join("tauqe.toml");
    }
    if let Some(cfg) = dirs_config() {
        return cfg.join("tauqe").join("tauqe.toml");
    }
    PathBuf::from("tauqe.toml")
}

pub fn default_credentials_path(repo_root: Option<&Path>) -> PathBuf {
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

pub fn has_openrouter_key(config: &AppConfig) -> bool {
    config
        .providers
        .openrouter
        .as_ref()
        .and_then(|o| o.api_key.as_deref())
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false)
}

pub fn write_default_config(path: &Path, model: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let trimmed_model = model.trim();
    let chosen_model = if trimmed_model.is_empty() {
        BUILTIN_DEFAULT_MODEL
    } else {
        trimmed_model
    };

    let available = vec![
        chosen_model.to_string(),
        "anthropic/claude-3.7-sonnet".to_string(),
        "anthropic/claude-3.5-sonnet".to_string(),
        "openai/gpt-4o".to_string(),
        "deepseek/deepseek-chat".to_string(),
    ];
    let mut deduped = Vec::new();
    for m in available {
        if !deduped.contains(&m) {
            deduped.push(m);
        }
    }

    let available_toml = deduped
        .iter()
        .map(|m| format!("    \"{}\",", m))
        .collect::<Vec<_>>()
        .join("\n");

    let content = format!(
        r#"[models]
default = "{}"
available = [
{}
]

[edit]
workflow = "git"
protocol = "xml"
"#,
        chosen_model, available_toml
    );

    std::fs::write(path, content)
}

pub fn write_credentials_file(path: &Path, api_key: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let content = format!("[openrouter]\napi_key = \"{}\"\n", api_key.trim());
    std::fs::write(path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }

    Ok(())
}

pub fn write_credentials_stub(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let content = r#"# Tauqe credentials
# OpenRouter API key (get one at https://openrouter.ai/keys)
[openrouter]
api_key = "sk-or-v1-YOUR-KEY-HERE"
"#;
    std::fs::write(path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }

    Ok(())
}

pub fn load_config(repo_root: Option<&Path>) -> AppConfig {
    let mut config = find_config_file(repo_root)
        .and_then(|p| {
            match std::fs::read_to_string(&p) {
                Ok(content) => match toml::from_str::<AppConfig>(&content) {
                    Ok(cfg) => Some(cfg),
                    Err(err) => {
                        tracing::warn!("Failed to parse config file {:?}: {}", p, err);
                        None
                    }
                },
                Err(err) => {
                    tracing::warn!("Failed to read config file {:?}: {}", p, err);
                    None
                }
            }
        })
        .unwrap_or_default();

    // Load credentials from credentials.toml (.tauqe/credentials.toml or ~/.config/tauqe/credentials.toml)
    if let Some(creds) = load_credentials(repo_root) {
        if let Some(or_creds) = creds.openrouter {
            if let Some(key) = or_creds.api_key {
                let trimmed = key.trim();
                if !trimmed.is_empty() {
                    let mut or_cfg = config.providers.openrouter.unwrap_or_default();
                    or_cfg.api_key = Some(trimmed.to_string());
                    config.providers.openrouter = Some(or_cfg);
                }
            }
        }
    }

    // Fallback to environment variables if still unset
    if config
        .providers
        .openrouter
        .as_ref()
        .and_then(|o| o.api_key.as_ref())
        .is_none()
    {
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            if !key.trim().is_empty() {
                let mut or_cfg = config.providers.openrouter.unwrap_or_default();
                or_cfg.api_key = Some(key.trim().to_string());
                config.providers.openrouter = Some(or_cfg);
            }
        }
    }

    if let Ok(model) = std::env::var("TAUQE_MODEL") {
        if !model.trim().is_empty() {
            config.models.default = model.trim().to_string();
        }
    }

    config.models.normalize();

    config
}

fn dirs_config() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .or_else(|_| std::env::var("APPDATA"))
        .ok()
        .map(PathBuf::from)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

pub fn find_credentials_file(repo_root: Option<&Path>) -> Option<PathBuf> {
    find_credentials_file_internal(repo_root, dirs_home().as_deref(), dirs_config().as_deref())
}

pub(crate) fn find_credentials_file_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    let mut add_candidate = |p: PathBuf| {
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    };

    if let Some(root) = repo_root {
        add_candidate(root.join(".tauqe").join("credentials.toml"));
    }

    if let Ok(cwd) = std::env::current_dir() {
        add_candidate(cwd.join(".tauqe").join("credentials.toml"));
    }

    if let Some(cfg) = config_dir {
        add_candidate(cfg.join("tauqe").join("credentials.toml"));
    }

    if let Some(h) = home {
        add_candidate(h.join(".config").join("tauqe").join("credentials.toml"));
        add_candidate(h.join(".tauqe").join("credentials.toml"));
    }

    candidates.into_iter().find(|p| p.is_file())
}

pub fn load_credentials(repo_root: Option<&Path>) -> Option<Credentials> {
    load_credentials_internal(repo_root, dirs_home().as_deref(), dirs_config().as_deref())
}

pub(crate) fn load_credentials_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
) -> Option<Credentials> {
    let path = find_credentials_file_internal(repo_root, home, config_dir)?;
    read_credentials_file(&path)
}

fn read_credentials_file(path: &Path) -> Option<Credentials> {
    match std::fs::read_to_string(path) {
        Ok(content) => match toml::from_str::<Credentials>(&content) {
            Ok(creds) => Some(creds),
            Err(err) => {
                tracing::warn!("Failed to parse credentials file {:?}: {}", path, err);
                None
            }
        },
        Err(err) => {
            tracing::warn!("Failed to read credentials file {:?}: {}", path, err);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_structured_protocol_config() {
        let toml_str = r#"
[edit]
workflow = "git"
protocol = "structured"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.edit.workflow, "git");
        assert_eq!(config.edit.protocol, "structured");
        assert_eq!(config.edit.max_retries, 3);
    }

    #[test]
    fn test_edit_max_retries_override() {
        let config: AppConfig = toml::from_str("[edit]\nmax_retries = 0\n").unwrap();
        assert_eq!(config.edit.max_retries, 0);
    }

    #[test]
    fn test_single_model_backward_compat() {
        let toml_str = r#"
[models]
default = "openai/gpt-4o"
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        config.models.normalize();
        assert_eq!(config.models.default, "openai/gpt-4o");
        assert_eq!(config.models.available, vec!["openai/gpt-4o".to_string()]);
    }

    #[test]
    fn test_available_models_include_default() {
        let toml_str = r#"
[models]
default = "a/b"
available = ["c/d", "e/f", "c/d"]
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        config.models.normalize();
        assert_eq!(config.models.default, "a/b");
        assert_eq!(
            config.models.available,
            vec!["a/b".to_string(), "c/d".to_string(), "e/f".to_string()]
        );
    }

    #[test]
    fn test_available_without_default_uses_first() {
        let toml_str = r#"
[models]
available = ["x/y", "z/w"]
"#;
        let mut config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        config.models.normalize();
        assert_eq!(config.models.default, "x/y");
        assert_eq!(config.models.available.len(), 2);
    }

    #[test]
    fn test_context_pinned_config_list() {
        let toml_str = r#"
[context]
pinned = ["docs/Core.md", "README.md"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(
            config.context.pinned,
            vec!["docs/Core.md".to_string(), "README.md".to_string()]
        );
    }

    #[test]
    fn test_context_pinned_config_table() {
        let toml_str = r#"
[context.pinned]
files = ["docs/TUI.md"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.pinned, vec!["docs/TUI.md".to_string()]);
    }

    #[test]
    fn test_git_upstream_config() {
        let toml_str = r#"
[git]
upstream = "origin/master"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.git.upstream.as_deref(), Some("origin/master"));
    }

    #[test]
    fn test_context_discovery_config() {
        let toml_str = r#"
[context]
max_discovery_rounds = 8
max_auto_files_per_round = 15
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.max_discovery_rounds, 8);
        assert_eq!(config.context.max_auto_files_per_round, Some(15));

        let default_cfg = ContextConfig::default();
        assert_eq!(default_cfg.max_discovery_rounds, 5);
        assert_eq!(default_cfg.max_auto_files_per_round, None);
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.models.default, "anthropic/claude-3.5-sonnet");
        assert_eq!(config.edit.workflow, "git");
        assert_eq!(config.edit.protocol, "xml");
        assert_eq!(config.edit.max_retries, 3);
        assert!(config.providers.openrouter.is_none());
        assert!(config.toolchain.check_command.is_none());
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

        let creds = load_credentials_internal(Some(temp_dir.path()), None, None);
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

        let creds = load_credentials_internal(None, Some(temp_home.path()), None);
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
        );
        assert!(creds.is_some());
        assert_eq!(
            creds.unwrap().openrouter.unwrap().api_key.as_deref(),
            Some("sk-project-priority")
        );
    }

    #[test]
    fn test_load_config_with_project_credentials() {
        let temp_dir = tempfile::tempdir().unwrap();
        // tauqe.toml without api_key
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\ndefault = \"anthropic/claude-3.5-sonnet\"\n",
        )
        .unwrap();

        // .tauqe/credentials.toml
        let dot_wb = temp_dir.path().join(".tauqe");
        std::fs::create_dir_all(&dot_wb).unwrap();
        std::fs::write(
            dot_wb.join("credentials.toml"),
            "[openrouter]\napi_key = \"sk-credentials-key\"\n",
        )
        .unwrap();

        let config = load_config(Some(temp_dir.path()));
        assert_eq!(
            config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref()),
            Some("sk-credentials-key")
        );
    }

    
    #[test]
    fn test_write_and_find_default_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cfg_path = default_config_path(Some(temp_dir.path()));
        assert_eq!(cfg_path, temp_dir.path().join("tauqe.toml"));

        assert!(find_config_file(Some(temp_dir.path())).is_none());
        write_default_config(&cfg_path, "deepseek/deepseek-chat").unwrap();

        let found = find_config_file(Some(temp_dir.path()));
        assert_eq!(found, Some(cfg_path));

        let loaded = load_config(Some(temp_dir.path()));
        assert_eq!(loaded.models.default, "deepseek/deepseek-chat");
        assert!(loaded.models.available.contains(&"deepseek/deepseek-chat".to_string()));
    }

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
        });
        assert!(has_openrouter_key(&cfg));
    }

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

    #[test]
    fn test_credentials_override_tauqe_toml_api_key() {
        let temp_dir = tempfile::tempdir().unwrap();
        // tauqe.toml with an old key
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[providers.openrouter]\napi_key = \"old-insecure-key\"\n",
        )
        .unwrap();

        // .tauqe/credentials.toml with a new secret
        let dot_wb = temp_dir.path().join(".tauqe");
        std::fs::create_dir_all(&dot_wb).unwrap();
        std::fs::write(
            dot_wb.join("credentials.toml"),
            "[openrouter]\napi_key = \"new-secure-key\"\n",
        )
        .unwrap();

        let config = load_config(Some(temp_dir.path()));
        assert_eq!(
            config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref()),
            Some("new-secure-key")
        );
    }
}
