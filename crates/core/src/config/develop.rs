use serde::{Deserialize, Serialize};
use tauqe_protocol::{EditProtocolKind, PatchStrategy, PromptLayoutKind, WorkflowKind};

/// Development profile: workflow engine, edit protocol, patch strategy and
/// retry policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevelopConfig {
    #[serde(default)]
    pub workflow: WorkflowKind,
    #[serde(default)]
    pub protocol: EditProtocolKind,
    /// Strategy that decides how a change set addresses the lines it replaces.
    ///
    /// It is orthogonal to `protocol`: the transport decides how an edit is
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
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_max_replans")]
    pub max_replans: usize,
}

/// Backwards-compatible alias for the legacy `[edit]` configuration section.
pub type EditConfig = DevelopConfig;

fn default_max_retries() -> usize {
    3
}

fn default_max_replans() -> usize {
    2
}

impl Default for DevelopConfig {
    fn default() -> Self {
        Self {
            workflow: WorkflowKind::default(),
            protocol: EditProtocolKind::default(),
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
[develop]
workflow = "git"
protocol = "structured"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Failed to parse config");
        assert_eq!(config.develop.workflow, WorkflowKind::Git);
        assert_eq!(config.develop.protocol, EditProtocolKind::Structured);
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
        assert_eq!(config.develop.workflow, WorkflowKind::Naive);
        assert_eq!(config.develop.protocol, EditProtocolKind::Xml);
        assert_eq!(config.develop.max_retries, 1);
    }

    #[test]
    fn test_edit_max_retries_override() {
        let config: AppConfig = toml::from_str("[develop]\nmax_retries = 0\n").unwrap();
        assert_eq!(config.develop.max_retries, 0);
    }

    #[test]
    fn test_develop_max_replans_override() {
        let config: AppConfig = toml::from_str("[develop]\nmax_replans = 4\n").unwrap();
        assert_eq!(config.develop.max_replans, 4);
    }

    #[test]
    fn test_develop_patch_strategy_defaults_and_overrides() {
        let config: AppConfig = toml::from_str("[develop]\n").unwrap();
        assert_eq!(config.develop.patch_strategy, PatchStrategy::LineAnchor);

        let config: AppConfig =
            toml::from_str("[develop]\npatch_strategy = \"search_replace\"\n").unwrap();
        assert_eq!(config.develop.patch_strategy, PatchStrategy::SearchReplace);

        let config: AppConfig =
            toml::from_str("[develop]\npatch_strategy = \"line_anchor\"\n").unwrap();
        assert_eq!(config.develop.patch_strategy, PatchStrategy::LineAnchor);

        assert!(toml::from_str::<AppConfig>("[develop]\npatch_strategy = \"anchors\"\n").is_err());
    }

    #[test]
    fn test_develop_prompt_layout_key_override() {
        let config: AppConfig = toml::from_str("[develop]\n").expect("parse config");
        assert_eq!(config.develop.prompt_layout, None);

        let config: AppConfig =
            toml::from_str("[develop]\nprompt_layout = \"semantic\"\n").expect("parse config");
        assert_eq!(
            config.develop.prompt_layout,
            Some(PromptLayoutKind::Semantic)
        );

        let config: AppConfig =
            toml::from_str("[develop]\nprompt_layout = \"chronological\"\n").expect("parse config");
        assert_eq!(
            config.develop.prompt_layout,
            Some(PromptLayoutKind::Chronological)
        );

        assert!(toml::from_str::<AppConfig>("[develop]\nprompt_layout = \"auto\"\n").is_err());
    }
}
