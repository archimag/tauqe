use serde::{Deserialize, Serialize};

use crate::model::{ModelRef, ModelRolesSummary, ModelSelection};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigState {
    pub workflow: String,
    pub edit_protocol: String,
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<ModelRolesSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_workflows: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_edit_protocols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<ModelRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_protocol: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialsSaveParams {
    pub api_key: String,
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
            workflow: "git".to_string(),
            edit_protocol: "xml".to_string(),
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
            available_workflows: vec!["git".to_string()],
            available_edit_protocols: vec!["xml".to_string()],
            available_models: vec![ModelRef::openrouter("coder-model")],
        };

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"roles\""));
        assert!(json.contains("\"coder\""));
        let de: ConfigState = serde_json::from_str(&json).unwrap();
        assert_eq!(de.roles.unwrap().coder, ModelRef::openrouter("coder-model"));
    }
}
