use serde::{Deserialize, Serialize};

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
