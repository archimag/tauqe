use serde::{Deserialize, Serialize};

use crate::discussion::{
    deserialize_null_as_default, DiscussionContextRequest, DiscussionRequest, RepoSearchRequest,
};
use crate::edit::{EditOperation, EditOutcome, StructuredChangeProposal};

use super::effort::ReasoningEffort;
use super::reference::ModelRef;
use super::role::ModelRole;
use super::usage::ModelUsageInfo;

/// Input of a `model/ask` request.
///
/// The two mutually exclusive turn kinds are modeled by a tagged enum instead
/// of a struct with a free-form `prompt` and an optional `discussion`. A
/// request therefore always carries exactly one target: it can never smuggle
/// both a prompt and a discussion target, nor arrive with neither, so the
/// illegal combinations the struct admitted are no longer representable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelAskParams {
    /// Free-form interactive turn.
    ///
    /// An empty prompt continues the anchored discussion (when one is active)
    /// or is rejected with `INVALID_PARAMS` by the server.
    Prompt { text: String },
    /// Discussion turn addressed at a plan or review session, carrying the
    /// typed target, the focused items, and the developer comment.
    Discussion(DiscussionRequest),
}

/// How an in-flight cancellation should stop a running batch supervisor.
///
/// A soft stop lets the current member finish its bounded turn, verification,
/// and Git commit before the queue halts; a hard stop aborts the current member
/// immediately. Outside a batch the mode is irrelevant and cancellation is
/// always immediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelMode {
    /// Finish the current member, then stop without starting the next one.
    Soft,
    /// Abort the current member immediately and stop the batch.
    #[default]
    Hard,
}

/// Parameters for `model/cancel`.
///
/// The `mode` field is optional so clients that predate two-mode cancellation
/// keep sending an empty object and receive the default immediate abort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCancelParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CancelMode>,
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
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ModelRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_context: Option<ModelTurnPlanContext>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_batch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeltaEvent {
    pub operation_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelResultProposal {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub changes: Vec<StructuredChangeProposal>,
    #[serde(default, deserialize_with = "deserialize_context_request_specs")]
    pub context_requests: Vec<DiscussionContextRequest>,
    #[serde(
        default,
        alias = "search_requests",
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub repo_searches: Vec<RepoSearchRequest>,
    #[serde(default)]
    pub context_drops: Vec<String>,
    #[serde(default)]
    pub suggested_actions: Vec<String>,
}

/// Normalizes the `context_requests` field of [`ModelResultProposal`] into
/// typed, access-aware requests.
///
/// The edit-protocol schema encodes the requested access level in a compact
/// string (`"path"`, `"read_only:path"`, or `"editable:path"`), while a
/// round-tripped proposal carries explicit `{ "path", "access" }` objects. Both
/// forms are accepted so the model's requested access level is preserved in a
/// single [`DiscussionContextRequest`] representation instead of being dropped
/// into an ambiguous bare string.
fn deserialize_context_request_specs<'de, D>(
    deserializer: D,
) -> Result<Vec<DiscussionContextRequest>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ContextRequestSpec {
        Path(String),
        Detailed(DiscussionContextRequest),
    }

    let specs = Vec::<ContextRequestSpec>::deserialize(deserializer)?;
    Ok(specs
        .into_iter()
        .filter_map(|spec| match spec {
            ContextRequestSpec::Path(path) => DiscussionContextRequest::parse_spec(&path),
            ContextRequestSpec::Detailed(request) => request.normalized(),
        })
        .collect())
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
        #[serde(flatten)]
        outcome: EditOutcome,
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
    fn test_model_result_answer_serde() {
        let answer = ModelResult::Answer {
            text: "Hello, world!".to_string(),
        };
        let json = serde_json::to_string(&answer).unwrap();
        assert!(json.contains("\"kind\":\"answer\""));

        let de: ModelResult = serde_json::from_str(&json).unwrap();
        assert_eq!(de, answer);
    }

    #[test]
    fn test_model_ask_params_is_a_tagged_enum() {
        let prompt = ModelAskParams::Prompt {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&prompt).unwrap();
        assert_eq!(json, "{\"kind\":\"prompt\",\"text\":\"hello\"}");
        let de: ModelAskParams = serde_json::from_str(&json).unwrap();
        assert_eq!(de, prompt);

        let discussion = ModelAskParams::Discussion(DiscussionRequest {
            target: crate::discussion::DiscussionTarget::Plan {
                id: "p-1".to_string(),
            },
            focused_items: vec!["1".to_string()],
            comment: "refine".to_string(),
        });
        let json = serde_json::to_string(&discussion).unwrap();
        assert!(json.contains("\"kind\":\"discussion\""));
        assert!(json.contains("\"comment\":\"refine\""));
        let de: ModelAskParams = serde_json::from_str(&json).unwrap();
        assert_eq!(de, discussion);
    }

    #[test]
    fn test_model_result_edit_flattens_the_shared_outcome() {
        let result = ModelResult::Edit {
            summary: "Change compute".to_string(),
            edits: Vec::new(),
            proposal: None,
            outcome: EditOutcome {
                applied: true,
                error: None,
                changed_files: vec!["src/lib.rs".to_string()],
                commit_hash: Some("abc1234".to_string()),
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"kind\":\"edit\""));
        assert!(json.contains("\"applied\":true"));
        assert!(json.contains("\"commit_hash\":\"abc1234\""));
        assert!(
            !json.contains("outcome"),
            "the shared outcome must serialize inline, not nested: {json}"
        );
        let de: ModelResult = serde_json::from_str(&json).unwrap();
        assert_eq!(de, result);
    }

    #[test]
    fn test_model_result_proposal_repo_searches_serde() {
        let raw = r#"{
            "message": "Searching for usage",
            "changes": [],
            "context_requests": [],
            "repo_searches": [{"path": "crates/core", "regex": "struct PlanStorage"}],
            "context_drops": [],
            "suggested_actions": []
        }"#;
        let proposal: ModelResultProposal = serde_json::from_str(raw).unwrap();
        assert_eq!(proposal.repo_searches.len(), 1);
        assert_eq!(proposal.repo_searches[0].path, "crates/core");
        assert_eq!(proposal.repo_searches[0].regex, "struct PlanStorage");

        // Alias `search_requests` support
        let raw_alias = r#"{
            "message": "Alias search",
            "search_requests": [{"path": "crates/protocol", "regex": "enum LlmCommand"}]
        }"#;
        let proposal_alias: ModelResultProposal = serde_json::from_str(raw_alias).unwrap();
        assert_eq!(proposal_alias.repo_searches.len(), 1);
        assert_eq!(proposal_alias.repo_searches[0].path, "crates/protocol");
        assert_eq!(proposal_alias.repo_searches[0].regex, "enum LlmCommand");

        // Null tolerance
        let raw_null = r#"{
            "message": "Null search",
            "repo_searches": null
        }"#;
        let proposal_null: ModelResultProposal = serde_json::from_str(raw_null).unwrap();
        assert!(proposal_null.repo_searches.is_empty());
    }

    #[test]
    fn test_model_result_proposal_preserves_context_request_access() {
        let raw = r#"{"message":"Need files","changes":[],"context_requests":["src/a.rs","editable:src/b.rs"],"context_drops":[],"suggested_actions":[]}"#;
        let proposal: ModelResultProposal = serde_json::from_str(raw).unwrap();
        assert_eq!(proposal.context_requests.len(), 2);
        assert_eq!(proposal.context_requests[0].path, "src/a.rs");
        assert_eq!(
            proposal.context_requests[0].access,
            crate::context::ContextAccess::ReadOnly
        );
        assert_eq!(proposal.context_requests[1].path, "src/b.rs");
        assert_eq!(
            proposal.context_requests[1].access,
            crate::context::ContextAccess::Editable
        );
    }

    #[test]
    fn test_cancel_mode_and_model_cancel_params_serde() {
        assert_eq!(CancelMode::default(), CancelMode::Hard);
        assert_eq!(serde_json::to_string(&CancelMode::Soft).unwrap(), "\"soft\"");
        assert_eq!(serde_json::to_string(&CancelMode::Hard).unwrap(), "\"hard\"");
        assert_eq!(
            serde_json::from_str::<CancelMode>("\"soft\"").unwrap(),
            CancelMode::Soft
        );

        // Legacy clients send an empty object; the mode stays absent and the
        // server falls back to an immediate abort.
        let legacy: ModelCancelParams = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.mode, None);
        assert_eq!(serde_json::to_string(&legacy).unwrap(), "{}");

        let soft = ModelCancelParams {
            mode: Some(CancelMode::Soft),
        };
        let json = serde_json::to_string(&soft).unwrap();
        assert_eq!(json, "{\"mode\":\"soft\"}");
        let de: ModelCancelParams = serde_json::from_str(&json).unwrap();
        assert_eq!(de.mode, Some(CancelMode::Soft));
    }
}
