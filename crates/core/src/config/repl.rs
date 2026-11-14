use serde::{Deserialize, Serialize};
use tauqe_protocol::{PatchStrategy, PromptLayoutKind, WorkflowKind};

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
    ///
    /// It is orthogonal to protocol: the transport decides how an edit is
    /// encoded, while the strategy decides whether it addresses a line range
    /// through the text it echoes or through the opaque anchors rendered in
    /// context.
    #[serde(default)]
    pub patch_strategy: PatchStrategy,
    /// Prompt layout override for every turn of this project.
    ///
    /// The key is absent by default, because the layout then follows the turn
    /// intent: an interactive REPL turn builds a chronological prompt, while a
    /// read-only discussion and an isolated step keep the semantic order. A
    /// declared value pins one policy for every turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_layout: Option<PromptLayoutKind>,
    /// Retry budget of one turn: failed edit staging and verification healing.
    ///
    /// It bounds how often a single attempt may repair its own change set, and
    /// says nothing about how many attempts a model tier receives: that is the
    /// separate budget declared on each model entry under `[models]`.
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_max_replans")]
    pub max_replans: usize,
}

/// Backwards-compatible alias for the legacy `[edit]` configuration section.
pub type EditConfig = ReplConfig;

fn default_max_retries() -> usize {
    3
}

fn default_max_replans() -> usize {
    2
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            workflow: WorkflowKind::default(),
            patch_strategy: PatchStrategy::default(),
            prompt_layout: None,
            max_retries: default_max_retries(),
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
        let toml_str = r#"
[repl]
workflow = "git"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.repl.workflow, WorkflowKind::Git);
        assert_eq!(config.repl.max_retries, 3);
    }
    #[test]
    fn test_edit_alias_compatibility() {
        let toml_str = r#"
[edit]
workflow = "naive"
max_retries = 1
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.repl.workflow, WorkflowKind::Naive);
        assert_eq!(config.repl.max_retries, 1);
    }
    #[test]
    fn test_repl_max_retries_override() {
        let config: AppConfig = toml::from_str("[repl]\nmax_retries = 0\n").unwrap();
        assert_eq!(config.repl.max_retries, 0);
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

    #[test]
    fn test_repl_prompt_layout_key_override() {
        let config: AppConfig = toml::from_str("[repl]\n").expect("parse config");
        assert_eq!(config.repl.prompt_layout, None);

        let config: AppConfig =
            toml::from_str("[repl]\nprompt_layout = \"semantic\"\n").expect("parse config");
        assert_eq!(config.repl.prompt_layout, Some(PromptLayoutKind::Semantic));

        let config: AppConfig =
            toml::from_str("[repl]\nprompt_layout = \"chronological\"\n").expect("parse config");
        assert_eq!(
            config.repl.prompt_layout,
            Some(PromptLayoutKind::Chronological)
        );

        assert!(toml::from_str::<AppConfig>("[repl]\nprompt_layout = \"auto\"\n").is_err());
    }
}
