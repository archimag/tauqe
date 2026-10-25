use serde::{Deserialize, Serialize};

use super::effort::ReasoningEffort;
use super::entry::ModelRolesSummary;
use super::reference::ModelRef;
use super::role::ModelRole;

/// Target model selection: an abstract semantic role, an explicit model
/// reference, or an explicit role-list rung carrying the reasoning depth of
/// the configured entry the developer picked.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModelSelection {
    Role(ModelRole),
    Specific(ModelRef),
    /// Explicit rung of a role list: the model together with the reasoning
    /// effort of the configured entry, so no consumer re-derives the depth by
    /// matching the model against the configured role lists.
    Tier {
        model: ModelRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<ReasoningEffort>,
    },
}

impl std::fmt::Display for ModelSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelSelection::Role(role) => write!(f, "{}", role),
            ModelSelection::Specific(model) => write!(f, "{}", model),
            ModelSelection::Tier { model, effort } => match effort {
                Some(effort) => write!(f, "{} [{}]", model, effort),
                None => write!(f, "{}", model),
            },
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

    #[test]
    fn test_model_selection_tier_serde_display_and_hash() {
        // A tier carrying a reasoning depth round-trips through the untagged
        // wire form: both coordinates of the picked role entry travel together.
        let tier = ModelSelection::Tier {
            model: ModelRef::openrouter("anthropic/claude-3.5-sonnet"),
            effort: Some(ReasoningEffort::High),
        };
        let json = serde_json::to_string(&tier).unwrap();
        assert!(json.contains("\"effort\":\"high\""), "got: {json}");
        let de: ModelSelection = serde_json::from_str(&json).unwrap();
        assert_eq!(de, tier);

        // A tier without a depth serializes compactly and reads back with no
        // effort.
        let bare = ModelSelection::Tier {
            model: ModelRef::openrouter("google/gemini-flash-latest"),
            effort: None,
        };
        let bare_json = serde_json::to_string(&bare).unwrap();
        assert!(!bare_json.contains("effort"), "got: {bare_json}");
        let de_bare: ModelSelection = serde_json::from_str(&bare_json).unwrap();
        assert_eq!(de_bare, bare);

        // The established forms keep decoding unchanged: a role string and a
        // structured model reference never collide with the tier shape.
        assert_eq!(
            serde_json::from_str::<ModelSelection>("\"coder\"").unwrap(),
            ModelSelection::Role(ModelRole::Coder)
        );
        let specific_json =
            serde_json::to_string(&ModelSelection::Specific(ModelRef::openrouter("a/b"))).unwrap();
        assert_eq!(
            serde_json::from_str::<ModelSelection>(&specific_json).unwrap(),
            ModelSelection::Specific(ModelRef::openrouter("a/b"))
        );

        // Display renders the model plus the bracketed depth, mirroring the
        // single interface label rule.
        assert_eq!(tier.to_string(), "openrouter:anthropic/claude-3.5-sonnet [high]");
        assert_eq!(bare.to_string(), "openrouter:google/gemini-flash-latest");

        // Equality and hashing distinguish tiers by both coordinates.
        let mut tiers = std::collections::HashSet::new();
        tiers.insert(tier.clone());
        assert!(!tiers.insert(tier), "an equal tier is already present");
        tiers.insert(bare);
        assert_eq!(tiers.len(), 2);
    }
}
