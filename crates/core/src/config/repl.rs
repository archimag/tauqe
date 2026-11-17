use serde::{Deserialize, Serialize};
use tauqe_protocol::{PatchStrategy, WorkflowKind};

/// Default budget of LLM rounds granted to a single turn.
pub const DEFAULT_MAX_TURN_ROUNDS: usize = 16;

/// Turn profile of a project: workflow engine, patch strategy,
/// and the retry and replanning budgets of a turn.
///
/// The profile is declared under the canonical `[repl]` section and governs
/// every bounded turn of the project: an isolated step and a discussion run
/// under the same workflow, patch strategy, and budgets as an
/// interactive REPL turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplConfig {
    #[serde(default)]
    pub workflow: WorkflowKind,
    /// Strategy that decides how a change set addresses the lines it replaces.
    #[serde(default)]
    pub patch_strategy: PatchStrategy,
    /// Total budget of LLM rounds granted to a single turn: research, staging retries,
    /// verification healing, and empty response retries all spend from this pool.
    #[serde(default = "default_max_turn_rounds")]
    pub(crate) max_turn_rounds: usize,
    #[serde(default = "default_max_replans")]
    pub max_replans: usize,
}

fn default_max_turn_rounds() -> usize {
    DEFAULT_MAX_TURN_ROUNDS
}

fn default_max_replans() -> usize {
    2
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            workflow: WorkflowKind::default(),
            patch_strategy: PatchStrategy::default(),
            max_turn_rounds: default_max_turn_rounds(),
            max_replans: default_max_replans(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn test_structured_protocol_config() {
        let toml_str = "[repl]\nworkflow = \"git\"\n";
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.repl.workflow, WorkflowKind::Git);
        assert_eq!(config.repl.max_turn_rounds, DEFAULT_MAX_TURN_ROUNDS);
    }

    #[test]
    fn test_repl_max_turn_rounds_override() {
        let config: AppConfig = toml::from_str("[repl]\nmax_turn_rounds = 42\n").unwrap();
        assert_eq!(config.repl.max_turn_rounds, 42);
    }

    #[test]
    fn test_repl_max_replans_override() {
        let config: AppConfig = toml::from_str("[repl]\nmax_replans = 4\n").unwrap();
        assert_eq!(config.repl.max_replans, 4);
    }

    #[test]
    fn test_repl_patch_strategy_defaults_and_overrides() {
        let config: AppConfig = toml::from_str("[repl]\n").unwrap();
        assert_eq!(config.repl.patch_strategy, PatchStrategy::LineAnchor);

        let config: AppConfig =
            toml::from_str("[repl]\npatch_strategy = \"search_replace\"\n").unwrap();
        assert_eq!(config.repl.patch_strategy, PatchStrategy::SearchReplace);

        let config: AppConfig =
            toml::from_str("[repl]\npatch_strategy = \"line_anchor\"\n").unwrap();
        assert_eq!(config.repl.patch_strategy, PatchStrategy::LineAnchor);

        assert!(toml::from_str::<AppConfig>("[repl]\npatch_strategy = \"anchors\"\n").is_err());
    }
}
