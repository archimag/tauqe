use serde::{Deserialize, Serialize};

use super::entry::ModelRolesSummary;
use super::reference::ModelRef;
use super::role::ModelRole;

/// Target model selection: either an abstract semantic role or an explicit model reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModelSelection {
    Role(ModelRole),
    Specific(ModelRef),
}

impl std::fmt::Display for ModelSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelSelection::Role(role) => write!(f, "{}", role),
            ModelSelection::Specific(model) => write!(f, "{}", model),
        }
    }
}

impl Default for ModelSelection {
    fn default() -> Self {
        // `Primary` is the mandatory base role and the deterministic default
        // for every specialized role that omits its own list, so it is the
        // only safe implicit selection.
        ModelSelection::Role(ModelRole::Primary)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelectParams {
    pub selection: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelectResult {
    pub selection: ModelSelection,
    pub effective_model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<ModelRolesSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCurrentResult {
    pub selection: ModelSelection,
    pub effective_model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<ModelRolesSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<ModelRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChangedEvent {
    pub selection: ModelSelection,
    pub effective_model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<ModelRolesSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_role_and_selection_serde() {
        let role = ModelRole::Coder;
        let role_json = serde_json::to_string(&role).unwrap();
        assert_eq!(role_json, "\"coder\"");
        assert_eq!(
            serde_json::from_str::<ModelRole>("\"coder\"").unwrap(),
            ModelRole::Coder
        );
        // serde routes through FromStr, so wire parsing matches configuration
        // parsing: trimmed and case-insensitive.
        assert_eq!(
            serde_json::from_str::<ModelRole>("\"Coder\"").unwrap(),
            ModelRole::Coder
        );
        assert_eq!(
            serde_json::from_str::<ModelSelection>("\"Architect\"").unwrap(),
            ModelSelection::Role(ModelRole::Architect)
        );
        assert!(serde_json::from_str::<ModelRole>("\"senior\"").is_err());

        let sel_role = ModelSelection::Role(ModelRole::Primary);
        let sel_role_json = serde_json::to_string(&sel_role).unwrap();
        assert_eq!(sel_role_json, "\"primary\"");
        assert_eq!(
            serde_json::from_str::<ModelSelection>("\"primary\"").unwrap(),
            sel_role
        );

        let specific = ModelRef::openrouter("anthropic/claude-3.5-sonnet");
        let sel_spec = ModelSelection::Specific(specific.clone());
        let sel_spec_json = serde_json::to_string(&sel_spec).unwrap();
        assert!(sel_spec_json.contains("\"provider\":\"openrouter\""));
        assert_eq!(
            serde_json::from_str::<ModelSelection>(&sel_spec_json).unwrap(),
            sel_spec
        );

        let roles_summary = ModelRolesSummary {
            scribe: ModelRef::openrouter("s"),
            coder: ModelRef::openrouter("c"),
            architect: ModelRef::openrouter("a"),
            primary: ModelRef::openrouter("ad"),
            reviewer: ModelRef::openrouter("rev"),
            efforts: None,
            entries: None,
        };
        let roles_json = serde_json::to_string(&roles_summary).unwrap();
        let de_roles: ModelRolesSummary = serde_json::from_str(&roles_json).unwrap();
        assert_eq!(de_roles, roles_summary);
    }

    #[test]
    fn test_model_selection_default_is_primary_role() {
        assert_eq!(
            ModelSelection::default(),
            ModelSelection::Role(ModelRole::Primary)
        );
    }
}
