use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauqe_protocol::{ModelRef, ModelSelection, ModelTier, ModelTiersSummary};

pub fn parse_model_ref(s: &str) -> Result<ModelRef, String> {
    let s = s.trim();
    if let Some((provider_str, model_name)) = s.split_once(':') {
        let provider_str = provider_str.trim().to_lowercase();
        let model_name = model_name.trim();
        if provider_str == "openrouter" {
            if model_name.is_empty() {
                return Err("Model name cannot be empty".to_string());
            }
            Ok(ModelRef::openrouter(model_name))
        } else {
            Err(format!("Unsupported provider '{}'", provider_str))
        }
    } else if !s.is_empty() {
        Ok(ModelRef::openrouter(s))
    } else {
        Err("Model specification cannot be empty".to_string())
    }
}

pub fn deserialize_model_ref_opt<'de, D>(deserializer: D) -> Result<Option<ModelRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ModelRefRaw {
        Str(String),
        Struct(ModelRef),
    }

    match Option::<ModelRefRaw>::deserialize(deserializer)? {
        None => Ok(None),
        Some(ModelRefRaw::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                parse_model_ref(trimmed).map(Some).map_err(serde::de::Error::custom)
            }
        }
        Some(ModelRefRaw::Struct(m)) => Ok(Some(m)),
    }
}

pub fn serialize_model_ref_opt<S>(opt: &Option<ModelRef>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match opt {
        Some(m) => serializer.serialize_str(&m.to_string()),
        None => serializer.serialize_none(),
    }
}

fn default_model_tier() -> ModelTier {
    ModelTier::Middle
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub junior: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub middle: Option<ModelRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_model_ref_opt",
        serialize_with = "serialize_model_ref_opt"
    )]
    pub senior: Option<ModelRef>,
    #[serde(default = "default_model_tier")]
    pub default: ModelTier,
    #[serde(default)]
    pub auto_level_up: bool,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            junior: None,
            middle: None,
            senior: None,
            default: ModelTier::Middle,
            auto_level_up: false,
        }
    }
}

impl ModelsConfig {
    /// Resolves a ModelTier using the deterministic fallback chain within the models configuration.
    pub fn resolve_tier(&self, tier: ModelTier) -> ModelRef {
        self.resolve_tier_with_fallback(tier, None)
    }

    /// Resolves a ModelTier with an optional external fallback model (e.g. from configured provider models).
    pub fn resolve_tier_with_fallback(
        &self,
        tier: ModelTier,
        fallback: Option<ModelRef>,
    ) -> ModelRef {
        match tier {
            ModelTier::Junior => self
                .junior
                .clone()
                .or_else(|| self.middle.clone())
                .or_else(|| self.senior.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_JUNIOR_MODEL)),
            ModelTier::Middle => self
                .middle
                .clone()
                .or_else(|| self.senior.clone())
                .or_else(|| self.junior.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_MIDDLE_MODEL)),
            ModelTier::Senior => self
                .senior
                .clone()
                .or_else(|| self.middle.clone())
                .or_else(|| self.junior.clone())
                .or(fallback)
                .unwrap_or_else(|| ModelRef::openrouter(BUILTIN_DEFAULT_SENIOR_MODEL)),
        }
    }

    /// Resolves a ModelSelection to a concrete ModelRef.
    pub fn resolve_model(&self, selection: &ModelSelection) -> ModelRef {
        match selection {
            ModelSelection::Tier(tier) => self.resolve_tier(*tier),
            ModelSelection::Specific(model) => model.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub models: ModelsConfig,
    #[serde(default, alias = "edit")]
    pub develop: DevelopConfig,
    #[serde(default)]
    pub toolchain: ToolchainConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub history: HistoryConfig,
}

fn default_history_budget_tokens() -> u64 {
    crate::history::DEFAULT_HISTORY_BUDGET_TOKENS
}

fn default_tail_turns() -> usize {
    crate::history::DEFAULT_TAIL_TURNS_COUNT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryConfig {
    #[serde(default = "default_history_budget_tokens")]
    pub budget_tokens: u64,
    #[serde(default = "default_tail_turns")]
    pub tail_turns: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            budget_tokens: default_history_budget_tokens(),
            tail_turns: default_tail_turns(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GitConfig {
    #[serde(default)]
    pub upstream: Option<String>,
}

pub const DEFAULT_MAX_DISCOVERY_ROUNDS: usize = 5;
pub const DEFAULT_MAX_CONTEXT_FILES: usize = 25;

/// Strategy for managing the auto-context layer during multi-round Discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryMode {
    /// Pure append-only context growth (original behavior); files accumulate monotonically until turn end.
    #[default]
    Monotonic,
    /// Additive layer expansion with explicit drops via `<context_drop path="..." />`.
    Incremental,
    /// Explicit declaration of the full working set each round; omitted auto-files are evicted.
    Snapshot,
}

impl std::fmt::Display for DiscoveryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Monotonic => write!(f, "monotonic"),
            Self::Incremental => write!(f, "incremental"),
            Self::Snapshot => write!(f, "snapshot"),
        }
    }
}

impl std::str::FromStr for DiscoveryMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "monotonic" | "cumulative" | "append_only" => Ok(Self::Monotonic),
            "incremental" => Ok(Self::Incremental),
            "snapshot" => Ok(Self::Snapshot),
            other => Err(format!(
                "Unknown discovery mode '{}'. Expected 'monotonic', 'incremental', or 'snapshot'",
                other
            )),
        }
    }
}

fn default_discovery_mode() -> DiscoveryMode {
    DiscoveryMode::Monotonic
}

fn default_max_discovery_rounds() -> usize {
    DEFAULT_MAX_DISCOVERY_ROUNDS
}

fn default_max_files() -> usize {
    DEFAULT_MAX_CONTEXT_FILES
}

fn default_repomap_token_budget() -> usize {
    crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default, deserialize_with = "deserialize_pinned")]
    pub pinned: Vec<String>,
    #[serde(default = "default_max_discovery_rounds")]
    pub max_discovery_rounds: usize,
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    #[serde(default = "default_repomap_token_budget")]
    pub repomap_token_budget: usize,
    #[serde(default = "default_discovery_mode")]
    pub discovery_mode: DiscoveryMode,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            max_discovery_rounds: DEFAULT_MAX_DISCOVERY_ROUNDS,
            max_files: DEFAULT_MAX_CONTEXT_FILES,
            repomap_token_budget: default_repomap_token_budget(),
            discovery_mode: DiscoveryMode::default(),
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
    /// In-memory API key loaded exclusively from credentials or environment.
    /// Never deserialized from or serialized into tauqe.toml.
    #[serde(skip)]
    pub api_key: Option<String>,
    /// Native OpenRouter model identifiers the user may switch between.
    #[serde(default)]
    pub models: Vec<String>,
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

pub const BUILTIN_DEFAULT_JUNIOR_MODEL: &str = "~anthropic/claude-haiku-latest";
pub const BUILTIN_DEFAULT_MIDDLE_MODEL: &str = "~google/gemini-flash-latest";
pub const BUILTIN_DEFAULT_SENIOR_MODEL: &str = "~anthropic/claude-sonnet-latest";
pub const BUILTIN_DEFAULT_MODEL: &str = BUILTIN_DEFAULT_MIDDLE_MODEL;

impl AppConfig {
    /// Models from provider sections, trimmed and de-duplicated.
    pub fn configured_models(&self) -> Vec<ModelRef> {
        let names = self
            .providers
            .openrouter
            .iter()
            .flat_map(|c| c.models.iter());

        let mut models = Vec::new();
        for name in names {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let model = ModelRef::openrouter(name);
            if !models.contains(&model) {
                models.push(model);
            }
        }
        models
    }

    /// Resolves a ModelTier to a concrete ModelRef using the deterministic fallback chain.
    /// If at least one model is configured in `models`, it covers all unspecified roles.
    pub fn resolve_tier(&self, tier: ModelTier) -> ModelRef {
        let fallback = self.configured_models().into_iter().next();
        self.models.resolve_tier_with_fallback(tier, fallback)
    }

    /// Resolves a ModelSelection to a concrete ModelRef.
    pub fn resolve_model(&self, selection: &ModelSelection) -> ModelRef {
        match selection {
            ModelSelection::Tier(tier) => self.resolve_tier(*tier),
            ModelSelection::Specific(model) => model.clone(),
        }
    }

    /// Summary of effective concrete models assigned to all tiers.
    pub fn tiers_summary(&self) -> ModelTiersSummary {
        ModelTiersSummary {
            junior: self.resolve_tier(ModelTier::Junior),
            middle: self.resolve_tier(ModelTier::Middle),
            senior: self.resolve_tier(ModelTier::Senior),
        }
    }

    /// The default active model based on `models.default`.
    pub fn active_model(&self) -> ModelRef {
        self.resolve_tier(self.models.default)
    }

    /// The default model selection based on `models.default`.
    pub fn default_selection(&self) -> ModelSelection {
        ModelSelection::Tier(self.models.default)
    }

    /// The model used for history summarization and squash commit message generation.
    pub fn history_model(&self) -> ModelRef {
        self.resolve_tier(ModelTier::Junior)
    }

    /// Models the user may switch between. Always contains all tier models and configured models.
    pub fn available_models(&self) -> Vec<ModelRef> {
        let mut models = self.configured_models();
        let summary = self.tiers_summary();
        for m in [summary.junior, summary.middle, summary.senior] {
            if !models.contains(&m) {
                models.push(m);
            }
        }
        models
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevelopConfig {
    #[serde(default = "default_workflow")]
    pub workflow: String, // "git", "naive"
    #[serde(default = "default_protocol")]
    pub protocol: String, // "xml", "structured"
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

pub type EditConfig = DevelopConfig;

fn default_workflow() -> String {
    "git".to_string()
}

fn default_protocol() -> String {
    "xml".to_string()
}

fn default_max_retries() -> usize {
    3
}

impl Default for DevelopConfig {
    fn default() -> Self {
        Self {
            workflow: default_workflow(),
            protocol: default_protocol(),
            max_retries: default_max_retries(),
        }
    }
}

/// Parses a command line option value supporting both `--flag value` and `--flag=value`.
pub fn cli_arg_value(flag: &str) -> Option<PathBuf> {
    cli_arg_value_from(std::env::args(), flag)
}

pub fn cli_arg_value_from<I, S>(args: I, flag: &str) -> Option<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let prefix = format!("{}=", flag);
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let arg_ref = arg.as_ref();
        if arg_ref == flag {
            if let Some(val) = iter.next() {
                let trimmed = val.as_ref().trim();
                if !trimmed.is_empty() {
                    return Some(PathBuf::from(trimmed));
                }
            }
        } else if let Some(val) = arg_ref.strip_prefix(&prefix) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

pub fn config_candidates(repo_root: Option<&Path>) -> Vec<PathBuf> {
    config_candidates_internal(repo_root, cli_arg_value("--config").as_deref())
}

pub(crate) fn config_candidates_internal(
    repo_root: Option<&Path>,
    cli_config: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(explicit) = cli_config {
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
    if let Some(cli_cfg) = cli_arg_value("--config") {
        return cli_cfg;
    }
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
    if let Some(cli_creds) = cli_arg_value("--credentials") {
        return cli_creds;
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

pub fn has_openrouter_key(config: &AppConfig) -> bool {
    config
        .providers
        .openrouter
        .as_ref()
        .and_then(|o| o.api_key.as_deref())
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false)
}

pub fn write_default_conventions(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if path.exists() {
        return Ok(());
    }
    let content = r#"# Project Conventions

## 0. Language & Tone
- All source code, identifiers, comments, documentation, and Git commit messages MUST be strictly in English.

## 1. Architectural Philosophy & Minimalism
- Strictly adhere to YAGNI (You Aren't Gonna Need It). Implement only what is required by current user goals.
- Prefer small, cohesive, deterministic modules over speculative abstractions.
- Do not invent unused configuration options, speculative features, or dead code.

## 2. Git Safety & Commits
- Formulate Git commit summaries in the imperative mood using Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`).
- Keep commits atomic and self-contained.

## 3. Verification & Quality Gates
- Ensure all changes compile cleanly and pass project build, test, and lint toolchains without warnings or regressions.
"#;
    std::fs::write(path, content)
}

pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../../../tauqe.toml");

pub fn write_default_config(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
        let conventions_path = if parent.as_os_str().is_empty() {
            PathBuf::from("Conventions.md")
        } else {
            parent.join("Conventions.md")
        };
        let _ = write_default_conventions(&conventions_path);
    } else {
        let _ = write_default_conventions(Path::new("Conventions.md"));
    }

    std::fs::write(path, DEFAULT_CONFIG_TEMPLATE)
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

/// Options for configuring how configuration and credentials files are resolved.
#[derive(Debug, Clone, Default)]
pub struct ConfigLoadOptions<'a> {
    pub repo_root: Option<&'a Path>,
    pub explicit_config: Option<&'a Path>,
    pub explicit_credentials: Option<&'a Path>,
}

pub fn load_config_with_options(opts: ConfigLoadOptions) -> AppConfig {
    let config_path = opts
        .explicit_config
        .map(PathBuf::from)
        .or_else(|| find_config_file(opts.repo_root));

    let mut config = config_path
        .and_then(|p| match std::fs::read_to_string(&p) {
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
        })
        .unwrap_or_default();

    // Load credentials from credentials.toml (.tauqe/credentials.toml, user config dir, or explicit CLI path)
    let creds = opts
        .explicit_credentials
        .and_then(read_credentials_file)
        .or_else(|| load_credentials(opts.repo_root));

    if let Some(creds) = creds {
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
                let mut or_cfg = config.providers.openrouter.unwrap_or_default();
                or_cfg.api_key = Some(trimmed.to_string());
                config.providers.openrouter = Some(or_cfg);
            }
        }
    }

    config
}

pub fn load_config(repo_root: Option<&Path>) -> AppConfig {
    let cli_config = cli_arg_value("--config");
    let cli_credentials = cli_arg_value("--credentials");
    load_config_with_options(ConfigLoadOptions {
        repo_root,
        explicit_config: cli_config.as_deref(),
        explicit_credentials: cli_credentials.as_deref(),
    })
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
    let cli_path = cli_arg_value("--credentials");
    find_credentials_file_internal(
        repo_root,
        dirs_home().as_deref(),
        dirs_config().as_deref(),
        cli_path.as_deref(),
    )
}

pub(crate) fn find_credentials_file_internal(
    repo_root: Option<&Path>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    cli_credentials: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(explicit) = cli_credentials {
        if !explicit.as_os_str().is_empty() && explicit.is_file() {
            return Some(explicit.to_path_buf());
        }
    }

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
    let cli_path = cli_arg_value("--credentials");
    load_credentials_internal(
        repo_root,
        dirs_home().as_deref(),
        dirs_config().as_deref(),
        cli_path.as_deref(),
    )
}

pub(crate) fn load_credentials_internal(
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
    fn test_cli_arg_parsing() {
        let args = vec![
            "tauqe-server".to_string(),
            "--config".to_string(),
            "/custom/tauqe.toml".to_string(),
            "--credentials=/custom/creds.toml".to_string(),
        ];

        let cfg = cli_arg_value_from(&args, "--config");
        assert_eq!(cfg, Some(PathBuf::from("/custom/tauqe.toml")));

        let creds = cli_arg_value_from(&args, "--credentials");
        assert_eq!(creds, Some(PathBuf::from("/custom/creds.toml")));

        let missing = cli_arg_value_from(&args, "--unknown");
        assert_eq!(missing, None);
    }

    #[test]
    fn test_cli_config_precedence() {
        let explicit = Path::new("/explicit/path/tauqe.toml");
        let candidates = config_candidates_internal(Some(Path::new("/repo")), Some(explicit));
        assert_eq!(candidates, vec![explicit.to_path_buf()]);
    }

    #[test]
    fn test_structured_protocol_config() {
        let toml_str = r#"
[develop]
workflow = "git"
protocol = "structured"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.develop.workflow, "git");
        assert_eq!(config.develop.protocol, "structured");
        assert_eq!(config.develop.max_retries, 3);
    }

    #[test]
    fn test_edit_alias_compatibility() {
        let toml_str = r#"
[edit]
workflow = "naive"
protocol = "xml"
max_retries = 1
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.develop.workflow, "naive");
        assert_eq!(config.develop.protocol, "xml");
        assert_eq!(config.develop.max_retries, 1);
    }

    #[test]
    fn test_edit_max_retries_override() {
        let config: AppConfig = toml::from_str("[develop]\nmax_retries = 0\n").unwrap();
        assert_eq!(config.develop.max_retries, 0);
    }

    #[test]
    fn test_models_config_and_tiers_resolution() {
        let toml_str = r#"
[models]
junior = "openrouter:anthropic/claude-haiku-latest"
middle = "openrouter:google/gemini-flash-latest"
senior = "openrouter:anthropic/claude-sonnet-latest"
default = "senior"
auto_level_up = true
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.models.default, ModelTier::Senior);
        assert!(config.models.auto_level_up);
        assert_eq!(config.active_model(), ModelRef::openrouter("anthropic/claude-sonnet-latest"));
        assert_eq!(config.history_model(), ModelRef::openrouter("anthropic/claude-haiku-latest"));

        let summary = config.tiers_summary();
        assert_eq!(summary.junior, ModelRef::openrouter("anthropic/claude-haiku-latest"));
        assert_eq!(summary.middle, ModelRef::openrouter("google/gemini-flash-latest"));
        assert_eq!(summary.senior, ModelRef::openrouter("anthropic/claude-sonnet-latest"));

        let specific = ModelRef::openrouter("custom/model");
        assert_eq!(config.resolve_model(&ModelSelection::Specific(specific.clone())), specific);
        assert_eq!(config.resolve_model(&ModelSelection::Tier(ModelTier::Junior)), summary.junior);
    }

    #[test]
    fn test_single_model_fallback_closes_all_roles() {
        let toml_str = r#"
[models]
middle = "openrouter:anthropic/claude-3.7-sonnet"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        let expected = ModelRef::openrouter("anthropic/claude-3.7-sonnet");
        assert_eq!(config.resolve_tier(ModelTier::Junior), expected);
        assert_eq!(config.resolve_tier(ModelTier::Middle), expected);
        assert_eq!(config.resolve_tier(ModelTier::Senior), expected);
    }

    #[test]
    fn test_available_models_include_all_tiers() {
        let toml_str = r#"
[models]
junior = "openrouter:a/junior"
middle = "openrouter:b/middle"
senior = "openrouter:c/senior"

[providers.openrouter]
models = ["d/extra", "a/junior"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        let available = config.available_models();
        assert!(available.contains(&ModelRef::openrouter("d/extra")));
        assert!(available.contains(&ModelRef::openrouter("a/junior")));
        assert!(available.contains(&ModelRef::openrouter("b/middle")));
        assert!(available.contains(&ModelRef::openrouter("c/senior")));
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
    fn test_history_config_defaults_and_override() {
        let default_cfg = HistoryConfig::default();
        assert_eq!(default_cfg.budget_tokens, 20_000);
        assert_eq!(default_cfg.tail_turns, 10);

        let toml_str = r#"
[history]
budget_tokens = 45000
tail_turns = 7
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.history.budget_tokens, 45_000);
        assert_eq!(config.history.tail_turns, 7);
    }

    #[test]
    fn test_repomap_budget_config() {
        let default_cfg = ContextConfig::default();
        assert_eq!(default_cfg.repomap_token_budget, 4000);

        let toml_str = r#"
[context]
repomap_token_budget = 6000
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.repomap_token_budget, 6000);
    }

    #[test]
    fn test_context_discovery_config() {
        let toml_str = r#"
[context]
max_discovery_rounds = 8
max_files = 40
discovery_mode = "snapshot"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.context.max_discovery_rounds, 8);
        assert_eq!(config.context.max_files, 40);
        assert_eq!(config.context.discovery_mode, DiscoveryMode::Snapshot);

        let default_cfg = ContextConfig::default();
        assert_eq!(default_cfg.max_discovery_rounds, 5);
        assert_eq!(default_cfg.max_files, 25);
        assert_eq!(default_cfg.discovery_mode, DiscoveryMode::Monotonic);

        assert_eq!(
            "monotonic".parse::<DiscoveryMode>().unwrap(),
            DiscoveryMode::Monotonic
        );
        assert_eq!(
            "cumulative".parse::<DiscoveryMode>().unwrap(),
            DiscoveryMode::Monotonic
        );
        assert_eq!(
            "append_only".parse::<DiscoveryMode>().unwrap(),
            DiscoveryMode::Monotonic
        );
    }

    #[test]
    fn test_typed_provider_models_config() {
        let toml_str = r#"
[models]
middle = "openrouter:openai/gpt-4o"

[providers.openrouter]
models = ["deepseek/deepseek-chat", " ", "openai/gpt-4o"]
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.active_model(), ModelRef::openrouter("openai/gpt-4o"));
        let available = config.available_models();
        assert!(available.contains(&ModelRef::openrouter("deepseek/deepseek-chat")));
        assert!(available.contains(&ModelRef::openrouter("openai/gpt-4o")));
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(
            config.active_model(),
            ModelRef::openrouter(BUILTIN_DEFAULT_MIDDLE_MODEL)
        );
        assert_eq!(config.develop.workflow, "git");
        assert_eq!(config.develop.protocol, "xml");
        assert_eq!(config.develop.max_retries, 3);
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
    fn test_load_config_with_project_credentials() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("tauqe.toml"),
            "[models]\nmiddle = \"openrouter:anthropic/claude-3.5-sonnet\"\n",
        )
        .unwrap();

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
    fn test_load_config_with_explicit_options() {
        let temp_dir = tempfile::tempdir().unwrap();
        let custom_cfg = temp_dir.path().join("my-custom.toml");
        std::fs::write(
            &custom_cfg,
            "[models]\nmiddle = \"openrouter:deepseek/deepseek-chat\"\n",
        )
        .unwrap();

        let custom_creds = temp_dir.path().join("my-creds.toml");
        std::fs::write(
            &custom_creds,
            "[openrouter]\napi_key = \"sk-explicit-creds\"\n",
        )
        .unwrap();

        let config = load_config_with_options(ConfigLoadOptions {
            repo_root: None,
            explicit_config: Some(&custom_cfg),
            explicit_credentials: Some(&custom_creds),
        });

        assert_eq!(
            config.active_model(),
            ModelRef::openrouter("deepseek/deepseek-chat")
        );
        assert_eq!(
            config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref()),
            Some("sk-explicit-creds")
        );
    }

    #[test]
    fn test_write_and_find_default_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cfg_path = default_config_path(Some(temp_dir.path()));
        assert_eq!(cfg_path, temp_dir.path().join("tauqe.toml"));

        assert!(find_config_file(Some(temp_dir.path())).is_none());
        write_default_config(&cfg_path).unwrap();

        let found = find_config_file(Some(temp_dir.path()));
        assert_eq!(found, Some(cfg_path));

        let loaded = load_config(Some(temp_dir.path()));
        let expected = ModelRef::openrouter("~google/gemini-flash-latest");
        assert_eq!(loaded.active_model(), expected);
        assert!(loaded.available_models().contains(&expected));
        assert!(loaded.context.pinned.contains(&"Conventions.md".to_string()));
        assert!(temp_dir.path().join("Conventions.md").is_file());
    }

    #[test]
    fn test_write_default_conventions_does_not_overwrite_existing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let conv_path = temp_dir.path().join("Conventions.md");
        std::fs::write(&conv_path, "custom conventions").unwrap();
        write_default_conventions(&conv_path).unwrap();
        let content = std::fs::read_to_string(&conv_path).unwrap();
        assert_eq!(content, "custom conventions");
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
            ..Default::default()
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
}
