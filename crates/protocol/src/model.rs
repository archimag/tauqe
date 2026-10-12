use serde::{Deserialize, Serialize};

use crate::edit::{EditOperation, StructuredChangeProposal};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[serde(rename = "openrouter")]
    OpenRouter,
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderKind::OpenRouter => f.write_str("openrouter"),
        }
    }
}

/// A model identified by its provider and the provider-native model name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: ProviderKind,
    pub name: String,
}

impl ModelRef {
    pub fn openrouter(name: impl Into<String>) -> Self {
        Self {
            provider: ProviderKind::OpenRouter,
            name: name.into(),
        }
    }
}

/// Presentation form only (`provider:name`); never parse it back.
impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider, self.name)
    }
}

/// Five semantic model role specializations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    #[serde(alias = "junior")]
    Scribe,
    #[serde(alias = "middle")]
    Coder,
    #[serde(alias = "senior")]
    Architect,
    Advisor,
    Reviewer,
}

pub type ModelTier = ModelRole;

impl std::fmt::Display for ModelRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelRole::Scribe => f.write_str("scribe"),
            ModelRole::Coder => f.write_str("coder"),
            ModelRole::Architect => f.write_str("architect"),
            ModelRole::Advisor => f.write_str("advisor"),
            ModelRole::Reviewer => f.write_str("reviewer"),
        }
    }
}

impl std::str::FromStr for ModelRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "scribe" | "junior" => Ok(ModelRole::Scribe),
            "coder" | "middle" => Ok(ModelRole::Coder),
            "architect" | "senior" => Ok(ModelRole::Architect),
            "advisor" => Ok(ModelRole::Advisor),
            "reviewer" => Ok(ModelRole::Reviewer),
            other => Err(format!(
                "Unknown model role '{}'. Expected scribe, coder, architect, advisor, or reviewer",
                other
            )),
        }
    }
}

/// Summary of effective concrete models assigned to each semantic role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRolesSummary {
    pub scribe: ModelRef,
    pub coder: ModelRef,
    pub architect: ModelRef,
    pub advisor: ModelRef,
    pub reviewer: ModelRef,
}

pub type ModelTiersSummary = ModelRolesSummary;

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
        ModelSelection::Role(ModelRole::Coder)
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAskParams {
    pub prompt: String,
}

/// Execution context of a model turn driven by a plan step.
///
/// Carries the owning plan identifier, the currently executing step, and the
/// compact ordered list of step identifiers that form the batch scope. This
/// allows clients to render a live plan stepper without switching views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelTurnPlanContext {
    pub plan_id: String,
    pub step_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub batch_step_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStartedEvent {
    pub operation_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_context: Option<ModelTurnPlanContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeltaEvent {
    pub operation_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelUsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageEvent {
    pub operation_id: String,
    pub usage: ModelUsageInfo,
    pub session_total_cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_cost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelResultProposal {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub changes: Vec<StructuredChangeProposal>,
    #[serde(default)]
    pub context_requests: Vec<String>,
    #[serde(default)]
    pub context_drops: Vec<String>,
    #[serde(default)]
    pub suggested_actions: Vec<String>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelResult {
    Answer {
        text: String,
    },
    Edit {
        summary: String,
        edits: Vec<EditOperation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal: Option<ModelResultProposal>,
        applied: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default)]
        changed_files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit_hash: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResultEvent {
    pub operation_id: String,
    pub result: ModelResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ModelUsageInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_total_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFinishedEvent {
    pub operation_id: String,
    pub full_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelErrorEvent {
    pub operation_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Discovery,
    Proposal,
    Staging,
    Verification,
    Healing,
}

impl std::fmt::Display for TurnPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnPhase::Discovery => f.write_str("discovery"),
            TurnPhase::Proposal => f.write_str("proposal"),
            TurnPhase::Staging => f.write_str("staging"),
            TurnPhase::Verification => f.write_str("verification"),
            TurnPhase::Healing => f.write_str("healing"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPhaseEvent {
    pub operation_id: String,
    pub phase: TurnPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMapInfoEvent {
    pub operation_id: String,
    pub tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolchainStartedEvent {
    pub operation_id: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolchainFinishedEvent {
    pub operation_id: String,
    pub command: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_ref_display_and_serde() {
        let model = ModelRef::openrouter("anthropic/claude-3.7-sonnet");
        assert_eq!(model.to_string(), "openrouter:anthropic/claude-3.7-sonnet");

        let json = serde_json::to_string(&model).unwrap();
        let de: ModelRef = serde_json::from_str(&json).unwrap();
        assert_eq!(de, model);
    }

    #[test]
    fn test_model_role_and_selection_serde() {
        let role = ModelRole::Coder;
        let role_json = serde_json::to_string(&role).unwrap();
        assert_eq!(role_json, "\"coder\"");
        assert_eq!(serde_json::from_str::<ModelRole>("\"coder\"").unwrap(), ModelRole::Coder);
        assert_eq!(serde_json::from_str::<ModelRole>("\"senior\"").unwrap(), ModelRole::Architect);
        assert_eq!(
            serde_json::from_str::<ModelSelection>("\"senior\"").unwrap(),
            ModelSelection::Role(ModelRole::Architect)
        );

        let sel_role = ModelSelection::Role(ModelRole::Advisor);
        let sel_role_json = serde_json::to_string(&sel_role).unwrap();
        assert_eq!(sel_role_json, "\"advisor\"");
        assert_eq!(serde_json::from_str::<ModelSelection>("\"advisor\"").unwrap(), sel_role);

        let specific = ModelRef::openrouter("anthropic/claude-3.5-sonnet");
        let sel_spec = ModelSelection::Specific(specific.clone());
        let sel_spec_json = serde_json::to_string(&sel_spec).unwrap();
        assert!(sel_spec_json.contains("\"provider\":\"openrouter\""));
        assert_eq!(serde_json::from_str::<ModelSelection>(&sel_spec_json).unwrap(), sel_spec);

        let roles_summary = ModelRolesSummary {
            scribe: ModelRef::openrouter("s"),
            coder: ModelRef::openrouter("c"),
            architect: ModelRef::openrouter("a"),
            advisor: ModelRef::openrouter("ad"),
            reviewer: ModelRef::openrouter("rev"),
        };
        let roles_json = serde_json::to_string(&roles_summary).unwrap();
        let de_roles: ModelRolesSummary = serde_json::from_str(&roles_json).unwrap();
        assert_eq!(de_roles, roles_summary);
    }

    #[test]
    fn test_model_result_answer_serde() {
        let answer = ModelResult::Answer {
            text: "Hello, world!".to_string(),
        };
        let json = serde_json::to_string(&answer).unwrap();
        assert!(json.contains("\"kind\":\"answer\""));

        let de: ModelResult = serde_json::from_str(&json).unwrap();
        assert_eq!(de, answer);
    }
}
