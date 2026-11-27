use serde::{Deserialize, Serialize};

pub const DEFAULT_INCLUDE_REPO_MAP: bool = true;

/// Default number of repository search queries honored in one Research round.
pub const DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND: usize = 5;

/// Default number of targeted repository maps rendered in one Research round.
pub const DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND: usize = 3;

fn default_include_repo_map() -> bool {
    DEFAULT_INCLUDE_REPO_MAP
}

fn default_max_search_requests_per_round() -> usize {
    DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND
}

fn default_max_repo_map_requests_per_round() -> usize {
    DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default, deserialize_with = "deserialize_pinned")]
    pub(crate) pinned: Vec<String>,
    #[serde(default = "default_include_repo_map")]
    pub include_repo_map: bool,
    /// Repository search queries honored in one Discovery round.
    #[serde(default = "default_max_search_requests_per_round")]
    pub(crate) max_search_requests_per_round: usize,
    /// Targeted repository maps rendered in one Discovery round.
    #[serde(default = "default_max_repo_map_requests_per_round")]
    pub(crate) max_repo_map_requests_per_round: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            include_repo_map: DEFAULT_INCLUDE_REPO_MAP,
            max_search_requests_per_round: DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND,
            max_repo_map_requests_per_round: DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_size_keys_are_ignored_and_not_serialized() {
        let config: ContextConfig = toml::from_str(
            r#"
            max_pattern_batch = 250
            token_budget = 1234
            files_token_budget = 4321
            repomap_token_budget = 500
            max_matched_files_per_search = 2
            max_dir_list_entries = 3
            max_feedback_chars = 4
            max_scratchpad_chars = 5
            "#,
        )
        .expect("retired keys remain ignored unknown fields");

        let serialized = toml::to_string(&config).expect("the configuration serializes");
        for key in [
            "token_budget",
            "repomap_token_budget",
            "max_matched_files_per_search",
            "max_dir_list_entries",
            "max_feedback_chars",
            "max_scratchpad_chars",
        ] {
            assert!(!serialized.contains(key), "got: {serialized}");
        }
    }
}
