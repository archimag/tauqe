use serde::{Deserialize, Serialize};

use crate::model::{ModelRef, ModelSelection, ModelTiersSummary};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigState {
    pub workflow: String,
    pub edit_protocol: String,
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<ModelTiersSummary>,
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
    pub tiers: Option<ModelTiersSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_model: Option<ModelRef>,
    pub available_models: Vec<ModelRef>,
}
