use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::model::{ModelRef, ModelRolesSummary, ModelSelection};

/// Discrete workflow engine that drives a development turn.
///
/// Replaces the former free-form workflow string. `Git` is the default
/// transactional workflow: the working tree is checkpointed and every applied
/// turn is committed. `Naive` applies edits directly to the working tree
/// without commit safety and is also known as the single-turn or direct mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum WorkflowKind {
    /// Transactional Git workflow (default).
    #[default]
    Git,
    /// Direct working-tree edits without commit checkpoints.
    Naive,
}

impl std::convert::TryFrom<String> for WorkflowKind {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl WorkflowKind {
    /// Canonical lower-case identifier used on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Naive => "naive",
        }
    }
}

impl fmt::Display for WorkflowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkflowKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "git" => Ok(Self::Git),
            // `single` and `direct` are legacy aliases for the naive (direct)
            // workflow; scheduled for removal in 0.2.0 and tracked by the
            // wire-contract test in `lib.rs`.
            "naive" | "single" | "direct" => Ok(Self::Naive),
            other => Err(format!(
                "Unknown workflow '{}'. Expected 'git' or 'naive'",
                other
            )),
        }
    }
}

/// Discrete edit protocol used to decode model output for a turn.
///
/// Replaces the former free-form protocol string. `Structured` is the default
/// and drives JSON-schema structured output, also known as the
/// structured-output or JSON-schema mode. `Xml` drives the marker-tag edit
/// protocol. `Mixed` is a two-contour mode that pairs both transports: XML for
/// code edits, structured output for discussions, commit messages, and history
/// compaction.
///
/// The choice is a transport decision only. The capabilities a turn exposes
/// follow from its intent and the active discussion focus, never from the
/// protocol, so both wire formats decode the same semantic command vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum EditProtocolKind {
    /// Marker-tag XML edit protocol.
    Xml,
    /// JSON-schema structured output protocol (default).
    #[default]
    Structured,
    /// Two-contour mode: XML for code edits, structured output elsewhere.
    Mixed,
}

impl std::convert::TryFrom<String> for EditProtocolKind {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl EditProtocolKind {
    /// Canonical lower-case identifier used on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Xml => "xml",
            Self::Structured => "structured",
            Self::Mixed => "mixed",
        }
    }
}

impl fmt::Display for EditProtocolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EditProtocolKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "xml" => Ok(Self::Xml),
            // `structured_output`, `json_schema`, and `json` are legacy aliases
            // for the structured protocol; scheduled for removal in 0.2.0 and
            // tracked by the wire-contract test in `lib.rs`.
            "structured" | "structured_output" | "json_schema" | "json" => Ok(Self::Structured),
            // `auto` is the historical name of the two-contour mode. It stays a
            // permanent compatibility alias, so a configuration written before
            // the rename keeps resolving to the same behavior instead of fading
            // into a second code path.
            "mixed" | "auto" => Ok(Self::Mixed),
            other => Err(format!(
                "Unknown edit protocol '{}'. Expected 'xml', 'structured' or 'mixed'",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigState {
    pub workflow: WorkflowKind,
    pub edit_protocol: EditProtocolKind,
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<ModelRolesSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_workflows: Vec<WorkflowKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_edit_protocols: Vec<EditProtocolKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<ModelRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_protocol: Option<EditProtocolKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_model: Option<ModelRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CredentialsSaveParams {
    pub api_key: String,
}

/// Manual `Debug` that never prints the plaintext API key.
///
/// A derived `Debug` on typed parameters would leak the secret into any
/// `tracing` span or `{:?}` log. Transport-level [`crate::message::Request`]
/// and [`crate::message::Message`] wrappers also provide dedicated `Debug`
/// redaction for methods with secrets (such as `credentials/save`). The key is
/// therefore replaced with a fixed placeholder.
impl std::fmt::Debug for CredentialsSaveParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialsSaveParams")
            .field("api_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatusResult {
    pub has_git: bool,
    pub has_config: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    pub default_config_path: String,
    pub has_api_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_path: Option<String>,
    pub default_credentials_path: String,
    pub ready: bool,
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<ModelRolesSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_model: Option<ModelRef>,
    pub available_models: Vec<ModelRef>,
    /// Diagnostic describing why configuration could not be fully loaded and
    /// built-in defaults were applied instead. Absent when loading succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelRef, ModelRole, ModelRolesSummary, ModelSelection};

    #[test]
    fn test_config_state_roles_serde() {
        let state = ConfigState {
            workflow: WorkflowKind::Git,
            edit_protocol: EditProtocolKind::Xml,
            model: ModelRef::openrouter("anthropic/claude-3.5-sonnet"),
            selection: Some(ModelSelection::Role(ModelRole::Coder)),
            roles: Some(ModelRolesSummary {
                scribe: ModelRef::openrouter("scribe-model"),
                coder: ModelRef::openrouter("coder-model"),
                architect: ModelRef::openrouter("architect-model"),
                primary: ModelRef::openrouter("primary-model"),
                reviewer: ModelRef::openrouter("reviewer-model"),
                efforts: None,
                entries: None,
            }),
            history_model: Some(ModelRef::openrouter("scribe-model")),
            available_workflows: vec![WorkflowKind::Git],
            available_edit_protocols: vec![EditProtocolKind::Xml],
            available_models: vec![ModelRef::openrouter("coder-model")],
        };

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"roles\""));
        assert!(json.contains("\"coder\""));
        let de: ConfigState = serde_json::from_str(&json).unwrap();
        assert_eq!(de.roles.unwrap().coder, ModelRef::openrouter("coder-model"));
        assert_eq!(de.workflow, WorkflowKind::Git);
        assert_eq!(de.edit_protocol, EditProtocolKind::Xml);
    }

    #[test]
    fn test_credentials_save_params_debug_redacts_key() {
        let params = CredentialsSaveParams {
            api_key: "sk-secret-value".to_string(),
        };
        let rendered = format!("{params:?}");
        assert!(!rendered.contains("sk-secret-value"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn test_workflow_kind_serde_and_aliases() {
        assert_eq!(
            serde_json::to_string(&WorkflowKind::Git).unwrap(),
            "\"git\""
        );
        assert_eq!(
            serde_json::to_string(&WorkflowKind::Naive).unwrap(),
            "\"naive\""
        );

        assert_eq!(
            serde_json::from_str::<WorkflowKind>("\"naive\"").unwrap(),
            WorkflowKind::Naive
        );
        assert_eq!(
            serde_json::from_str::<WorkflowKind>("\"single\"").unwrap(),
            WorkflowKind::Naive
        );
        assert_eq!(
            serde_json::from_str::<WorkflowKind>("\"direct\"").unwrap(),
            WorkflowKind::Naive
        );
        assert_eq!(
            serde_json::from_str::<WorkflowKind>("\" GIT \"").unwrap(),
            WorkflowKind::Git
        );
        assert_eq!(
            serde_json::from_str::<WorkflowKind>("\" DIRECT \"").unwrap(),
            WorkflowKind::Naive
        );
        assert!(serde_json::from_str::<WorkflowKind>("\"legacy\"").is_err());
    }

    #[test]
    fn test_workflow_kind_default_display_and_parsing() {
        assert_eq!(WorkflowKind::default(), WorkflowKind::Git);
        assert_eq!(WorkflowKind::Git.as_str(), "git");
        assert_eq!(WorkflowKind::Naive.to_string(), "naive");
        assert_eq!(" GIT ".parse::<WorkflowKind>().unwrap(), WorkflowKind::Git);
        assert_eq!(
            "direct".parse::<WorkflowKind>().unwrap(),
            WorkflowKind::Naive
        );
        assert!("".parse::<WorkflowKind>().is_err());
        assert!("bogus".parse::<WorkflowKind>().is_err());
    }

    #[test]
    fn test_edit_protocol_kind_serde_and_aliases() {
        assert_eq!(
            serde_json::to_string(&EditProtocolKind::Xml).unwrap(),
            "\"xml\""
        );
        assert_eq!(
            serde_json::to_string(&EditProtocolKind::Structured).unwrap(),
            "\"structured\""
        );
        assert_eq!(
            serde_json::to_string(&EditProtocolKind::Mixed).unwrap(),
            "\"mixed\""
        );

        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\"xml\"").unwrap(),
            EditProtocolKind::Xml
        );
        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\"structured_output\"").unwrap(),
            EditProtocolKind::Structured
        );
        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\"json_schema\"").unwrap(),
            EditProtocolKind::Structured
        );
        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\"json\"").unwrap(),
            EditProtocolKind::Structured
        );
        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\"mixed\"").unwrap(),
            EditProtocolKind::Mixed
        );
        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\"auto\"").unwrap(),
            EditProtocolKind::Mixed
        );
        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\" AUTO \"").unwrap(),
            EditProtocolKind::Mixed
        );
        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\" XML \"").unwrap(),
            EditProtocolKind::Xml
        );
        assert_eq!(
            serde_json::from_str::<EditProtocolKind>("\" JSON_SCHEMA \"").unwrap(),
            EditProtocolKind::Structured
        );
        assert!(serde_json::from_str::<EditProtocolKind>("\"yaml\"").is_err());
    }

    #[test]
    fn test_edit_protocol_kind_default_display_and_parsing() {
        assert_eq!(EditProtocolKind::default(), EditProtocolKind::Structured);
        assert_eq!(EditProtocolKind::Xml.as_str(), "xml");
        assert_eq!(EditProtocolKind::Structured.to_string(), "structured");
        assert_eq!(EditProtocolKind::Mixed.to_string(), "mixed");
        assert_eq!(
            "STRUCTURED".parse::<EditProtocolKind>().unwrap(),
            EditProtocolKind::Structured
        );
        assert_eq!(
            "json".parse::<EditProtocolKind>().unwrap(),
            EditProtocolKind::Structured
        );
        assert_eq!(
            "mixed".parse::<EditProtocolKind>().unwrap(),
            EditProtocolKind::Mixed
        );
        assert_eq!(
            " MIXED ".parse::<EditProtocolKind>().unwrap(),
            EditProtocolKind::Mixed
        );
        assert_eq!(
            "auto".parse::<EditProtocolKind>().unwrap(),
            EditProtocolKind::Mixed
        );
        assert_eq!(
            " AUTO ".parse::<EditProtocolKind>().unwrap(),
            EditProtocolKind::Mixed
        );
        assert!("".parse::<EditProtocolKind>().is_err());
    }
}
