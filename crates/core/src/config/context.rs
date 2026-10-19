use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_DISCOVERY_ROUNDS: usize = 5;
pub const DEFAULT_FILES_TOKEN_BUDGET: usize = 300_000;
pub const DEFAULT_MAX_PATTERN_BATCH: usize = 150;
pub const DEFAULT_INCLUDE_REPO_MAP: bool = true;

fn default_max_discovery_rounds() -> usize {
    DEFAULT_MAX_DISCOVERY_ROUNDS
}

fn default_files_token_budget() -> usize {
    DEFAULT_FILES_TOKEN_BUDGET
}

fn default_max_pattern_batch() -> usize {
    DEFAULT_MAX_PATTERN_BATCH
}

fn default_include_repo_map() -> bool {
    DEFAULT_INCLUDE_REPO_MAP
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
    #[serde(default = "default_files_token_budget")]
    pub files_token_budget: usize,
    #[serde(default = "default_repomap_token_budget")]
    pub repomap_token_budget: usize,
    #[serde(default = "default_include_repo_map")]
    pub include_repo_map: bool,
    #[serde(default = "default_max_pattern_batch")]
    pub max_pattern_batch: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            max_discovery_rounds: DEFAULT_MAX_DISCOVERY_ROUNDS,
            files_token_budget: DEFAULT_FILES_TOKEN_BUDGET,
            repomap_token_budget: default_repomap_token_budget(),
            include_repo_map: DEFAULT_INCLUDE_REPO_MAP,
            max_pattern_batch: DEFAULT_MAX_PATTERN_BATCH,
        }
    }
}

/// Deserializes `[context].pinned`.
///
/// Accepts a list of paths (`pinned = ["a.md", "b.md"]`), a single path
/// string, or a table carrying a `files` key (`[context.pinned]` with
/// `files = ["a.md"]`). Any other table is rejected with a descriptive error
/// rather than being silently reinterpreted, so a mistyped table cannot pin
/// arbitrary keys as paths.
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
        OtherTable(toml::Value),
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
        PinnedRaw::OtherTable(value) => {
            return Err(serde::de::Error::custom(format!(
                "invalid `[context].pinned` value: expected a list of paths, a single path, \
                 or a table with a `files` key, found {}",
                value.type_str()
            )));
        }
    }
    Ok(files)
}
