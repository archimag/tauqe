use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_DISCOVERY_ROUNDS: usize = 5;
pub const DEFAULT_FILES_TOKEN_BUDGET: usize = 300_000;
pub const DEFAULT_INCLUDE_REPO_MAP: bool = true;

/// Default number of repository search queries honored in one Discovery round.
pub const DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND: usize = 5;

/// Default number of matching files reported for a single search query.
pub const DEFAULT_MAX_MATCHED_FILES_PER_SEARCH: usize = 50;

/// Default number of entries rendered by a single directory listing.
pub const DEFAULT_MAX_DIR_LIST_ENTRIES: usize = 100;

/// Default number of targeted repository maps rendered in one Discovery round.
pub const DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND: usize = 3;

/// Default maximum characters of one project command stream reported back.
pub const DEFAULT_MAX_FEEDBACK_CHARS: usize = 4_000;

/// Default maximum characters of a turn scratchpad block kept before truncation.
pub const DEFAULT_MAX_SCRATCHPAD_CHARS: usize = 4_000;

/// Default total token budget of the lazily loaded external reference documents.
pub const DEFAULT_REFS_TOKEN_BUDGET: usize = 50_000;

fn default_max_discovery_rounds() -> usize {
    DEFAULT_MAX_DISCOVERY_ROUNDS
}

fn default_files_token_budget() -> usize {
    DEFAULT_FILES_TOKEN_BUDGET
}

fn default_include_repo_map() -> bool {
    DEFAULT_INCLUDE_REPO_MAP
}

fn default_repomap_token_budget() -> usize {
    crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET
}

fn default_max_search_requests_per_round() -> usize {
    DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND
}

fn default_max_matched_files_per_search() -> usize {
    DEFAULT_MAX_MATCHED_FILES_PER_SEARCH
}

fn default_max_dir_list_entries() -> usize {
    DEFAULT_MAX_DIR_LIST_ENTRIES
}

fn default_max_repo_map_requests_per_round() -> usize {
    DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND
}

fn default_max_feedback_chars() -> usize {
    DEFAULT_MAX_FEEDBACK_CHARS
}

fn default_max_scratchpad_chars() -> usize {
    DEFAULT_MAX_SCRATCHPAD_CHARS
}

fn default_refs_token_budget() -> usize {
    DEFAULT_REFS_TOKEN_BUDGET
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
    /// Repository search queries honored in one Discovery round.
    #[serde(default = "default_max_search_requests_per_round")]
    pub max_search_requests_per_round: usize,
    /// Matching files reported for a single search query.
    #[serde(default = "default_max_matched_files_per_search")]
    pub max_matched_files_per_search: usize,
    /// Entries rendered by a single directory listing.
    #[serde(default = "default_max_dir_list_entries")]
    pub max_dir_list_entries: usize,
    /// Targeted repository maps rendered in one Discovery round.
    #[serde(default = "default_max_repo_map_requests_per_round")]
    pub max_repo_map_requests_per_round: usize,
    /// Characters of one project command stream reported back to the model.
    #[serde(default = "default_max_feedback_chars")]
    pub max_feedback_chars: usize,
    /// Characters of a turn scratchpad block kept before safe truncation.
    #[serde(default = "default_max_scratchpad_chars")]
    pub max_scratchpad_chars: usize,
    /// Total token budget of the external reference documents kept in context.
    #[serde(default = "default_refs_token_budget")]
    pub refs_token_budget: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            max_discovery_rounds: DEFAULT_MAX_DISCOVERY_ROUNDS,
            files_token_budget: DEFAULT_FILES_TOKEN_BUDGET,
            repomap_token_budget: default_repomap_token_budget(),
            include_repo_map: DEFAULT_INCLUDE_REPO_MAP,
            max_search_requests_per_round: DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND,
            max_matched_files_per_search: DEFAULT_MAX_MATCHED_FILES_PER_SEARCH,
            max_dir_list_entries: DEFAULT_MAX_DIR_LIST_ENTRIES,
            max_repo_map_requests_per_round: DEFAULT_MAX_REPO_MAP_REQUESTS_PER_ROUND,
            max_feedback_chars: DEFAULT_MAX_FEEDBACK_CHARS,
            max_scratchpad_chars: DEFAULT_MAX_SCRATCHPAD_CHARS,
            refs_token_budget: DEFAULT_REFS_TOKEN_BUDGET,
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

    /// A project configuration written before the file-count limit was retired
    /// still decodes: the retired key stays an unknown key of `[context]`, and
    /// the file token budget remains the only measure of how much a project
    /// admits into the context.
    #[test]
    fn test_a_retired_max_pattern_batch_key_is_ignored() {
        let config: ContextConfig = toml::from_str(
            r#"
            max_pattern_batch = 250
            files_token_budget = 1234
            "#,
        )
        .expect("a retired key must not fail decoding");

        assert_eq!(config.files_token_budget, 1234);
        assert_eq!(
            config.max_search_requests_per_round,
            DEFAULT_MAX_SEARCH_REQUESTS_PER_ROUND
        );
    }
}
