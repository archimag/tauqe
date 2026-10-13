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

/// Parses a model reference from its configuration string form.
///
/// Accepts an optional `provider:` prefix (`openrouter:`) or a bare
/// provider-native name, which defaults to OpenRouter. Surrounding whitespace
/// is ignored.
impl std::str::FromStr for ModelRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("Model specification cannot be empty".to_string());
        }

        if let Some((provider_str, model_name)) = s.split_once(':') {
            let provider_str = provider_str.trim().to_lowercase();
            let model_name = model_name.trim();
            if provider_str == "openrouter" {
                if model_name.is_empty() {
                    return Err("Model name cannot be empty".to_string());
                }
                return Ok(ModelRef::openrouter(model_name));
            }
        }

        Ok(ModelRef::openrouter(s))
    }
}

/// Five semantic model role specializations.
///
/// The legacy spellings `junior`, `middle`, and `senior` are still accepted as
/// serde aliases and by the `FromStr` implementation. They are retained for a
/// migration window so persisted plans, reviews, and legacy client payloads
/// keep deserializing onto `Scribe`, `Coder`, and `Architect`; remove them once
/// no stored artifact or external client emits the legacy names.
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

/// Reasoning depth (cognitive effort) requested for a semantic role.
///
/// Kept strictly orthogonal to the model identity: the same model may be
/// driven at different reasoning depths depending on the role it plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    /// Canonical lowercase wire representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "low" => Ok(ReasoningEffort::Low),
            "medium" => Ok(ReasoningEffort::Medium),
            "high" => Ok(ReasoningEffort::High),
            "xhigh" => Ok(ReasoningEffort::XHigh),
            other => Err(format!(
                "Unknown reasoning effort '{}'. Expected low, medium, high, or xhigh",
                other
            )),
        }
    }
}

impl Serialize for ReasoningEffort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Deserializes an optional reasoning effort, treating blank strings as absence.
fn deserialize_effort_opt<'de, D>(deserializer: D) -> Result<Option<ReasoningEffort>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed.parse().map(Some).map_err(serde::de::Error::custom)
            }
        }
    }
}

/// Optional per-role reasoning effort overrides.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelRolesEffort {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub scribe: Option<ReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub coder: Option<ReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub architect: Option<ReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub advisor: Option<ReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_effort_opt"
    )]
    pub reviewer: Option<ReasoningEffort>,
}

/// A single rung of the coder escalation ladder.
///
/// Pairs a concrete model with an optional reasoning depth. Deserializes from
/// either a bare model string or a `{ model, effort }` table, so a ladder may
/// mix terse and detailed rungs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoderTier {
    pub model: ModelRef,
    pub effort: Option<ReasoningEffort>,
}

impl CoderTier {
    /// Creates a coder rung from a model and an optional reasoning depth.
    pub fn new(model: ModelRef, effort: Option<ReasoningEffort>) -> Self {
        Self { model, effort }
    }
}

impl From<ModelRef> for CoderTier {
    fn from(model: ModelRef) -> Self {
        Self {
            model,
            effort: None,
        }
    }
}

impl<'de> Deserialize<'de> for CoderTier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Table {
                model: String,
                #[serde(default, deserialize_with = "deserialize_effort_opt")]
                effort: Option<ReasoningEffort>,
            },
        }

        match Raw::deserialize(deserializer)? {
            Raw::Str(s) => {
                let model = s.parse::<ModelRef>().map_err(serde::de::Error::custom)?;
                Ok(CoderTier::new(model, None))
            }
            Raw::Table { model, effort } => {
                let model = model.parse::<ModelRef>().map_err(serde::de::Error::custom)?;
                Ok(CoderTier::new(model, effort))
            }
        }
    }
}

impl Serialize for CoderTier {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.effort {
            Some(effort) => {
                use serde::ser::SerializeStruct;
                let mut state = serializer.serialize_struct("CoderTier", 2)?;
                state.serialize_field("model", &self.model.to_string())?;
                state.serialize_field("effort", &effort)?;
                state.end()
            }
            None => serializer.serialize_str(&self.model.to_string()),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<ModelRolesEffort>,
}

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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_batch: bool,
}

/// Terminal status of a batch supervisor run.
///
/// Batch supervisors execute a bounded sequence of isolated turns (plan steps or
/// review findings). Their terminal state is distinct from a single turn's
/// outcome because a batch may stop early while already-completed members remain
/// committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchFinishStatus {
    /// Every scheduled member completed successfully.
    Completed,
    /// A member failed and the batch stopped early.
    Failed,
    /// A member emitted a semantic blocker and the batch stopped early.
    Blocked,
    /// The batch was cancelled by the user.
    Cancelled,
}

/// Compact summary of a completed batch run.
///
/// Carries the identifiers and progress counters shared by plan and review
/// batches so clients can render a single aggregated completion notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchFinishSummary {
    /// Identifier of the owning plan or review session.
    pub owner_id: String,
    pub status: BatchFinishStatus,
    /// Members executed before the batch stopped.
    pub executed_count: usize,
    /// Total members scheduled for the batch.
    pub total_count: usize,
    /// Wall-clock duration of the batch in seconds.
    pub elapsed_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Emitted once when a plan batch supervisor terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBatchFinishedEvent {
    pub operation_id: String,
    pub summary: BatchFinishSummary,
}

/// Emitted once when a review batch supervisor terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBatchFinishedEvent {
    pub operation_id: String,
    pub summary: BatchFinishSummary,
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
    fn test_model_ref_from_str() {
        let m1: ModelRef = "openrouter:anthropic/claude-3.7-sonnet".parse().unwrap();
        assert_eq!(m1, ModelRef::openrouter("anthropic/claude-3.7-sonnet"));

        let m2: ModelRef = "openrouter:meta-llama/llama-3.3-70b-instruct:free".parse().unwrap();
        assert_eq!(m2, ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:free"));

        let m3: ModelRef = "meta-llama/llama-3.3-70b-instruct:free".parse().unwrap();
        assert_eq!(m3, ModelRef::openrouter("meta-llama/llama-3.3-70b-instruct:free"));

        let m4: ModelRef = "anthropic/claude-3.7-sonnet".parse().unwrap();
        assert_eq!(m4, ModelRef::openrouter("anthropic/claude-3.7-sonnet"));

        assert!("".parse::<ModelRef>().is_err());
        assert!("   ".parse::<ModelRef>().is_err());
        assert!("openrouter:".parse::<ModelRef>().is_err());
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
            efforts: None,
        };
        let roles_json = serde_json::to_string(&roles_summary).unwrap();
        let de_roles: ModelRolesSummary = serde_json::from_str(&roles_json).unwrap();
        assert_eq!(de_roles, roles_summary);
    }

    #[test]
    fn test_reasoning_effort_parse_display_and_serde() {
        assert_eq!("low".parse::<ReasoningEffort>().unwrap(), ReasoningEffort::Low);
        assert_eq!(
            "  HIGH ".parse::<ReasoningEffort>().unwrap(),
            ReasoningEffort::High
        );
        assert_eq!(ReasoningEffort::XHigh.to_string(), "xhigh");
        assert!("extreme".parse::<ReasoningEffort>().is_err());

        let json = serde_json::to_string(&ReasoningEffort::XHigh).unwrap();
        assert_eq!(json, "\"xhigh\"");
        let de: ReasoningEffort = serde_json::from_str("\"Medium\"").unwrap();
        assert_eq!(de, ReasoningEffort::Medium);

        let efforts = ModelRolesEffort {
            architect: Some(ReasoningEffort::XHigh),
            ..Default::default()
        };
        let efforts_json = serde_json::to_string(&efforts).unwrap();
        assert_eq!(efforts_json, "{\"architect\":\"xhigh\"}");
        let de_efforts: ModelRolesEffort = serde_json::from_str(&efforts_json).unwrap();
        assert_eq!(de_efforts, efforts);

        let blank: ModelRolesEffort = serde_json::from_str("{\"coder\":\"\"}").unwrap();
        assert_eq!(blank.coder, None);
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
