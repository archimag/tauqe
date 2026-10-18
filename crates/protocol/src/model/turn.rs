use serde::{Deserialize, Serialize};

use crate::command::DocTopic;
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

/// Terminal outcome declared through a structured proposal.
///
/// Structured output carries the terminal outcome as an explicit object field
/// instead of a `<success>` / `<error>` control tag. `status` is kept as the raw
/// model-declared string so the decoding layer owns tolerant normalization,
/// while `summary`, `message`, and `reason` carry the optional success summary,
/// error text, or skip reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StructuredOutcomeProposal {
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl StructuredOutcomeProposal {
    /// Reports whether the status represents a skipped outcome ("skip" or "skipped").
    pub fn is_skip(&self) -> bool {
        let s = self.status.trim();
        s.eq_ignore_ascii_case("skip") || s.eq_ignore_ascii_case("skipped")
    }

    /// Reports whether the status represents a successful outcome ("success").
    pub fn is_success(&self) -> bool {
        self.status.trim().eq_ignore_ascii_case("success")
    }

    /// Reports whether the status represents an error outcome ("error").
    pub fn is_error(&self) -> bool {
        self.status.trim().eq_ignore_ascii_case("error")
    }
}

/// Semantic blocker declared through a structured proposal.
///
/// Mirrors the `<blocked>` protocol tag: `is_blocked` is the explicit flag the
/// model sets when a precondition or assumption fails, `reason` names the
/// violated precondition, and `details` optionally expands it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredBlockedProposal {
    #[serde(default)]
    pub is_blocked: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
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
    #[serde(
        default,
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub doc_requests: Vec<DocTopic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratchpad: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<StructuredOutcomeProposal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<StructuredBlockedProposal>,
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

    #[test]
    fn test_model_result_proposal_structured_control_fields() {
        let raw = r#"{
            "message": "Gathering context",
            "changes": [],
            "context_requests": [],
            "repo_searches": [],
            "doc_requests": ["workflow", "git"],
            "scratchpad": "Verified: parser fixed.",
            "outcome": {"status": "success", "summary": "All checks passed", "message": null},
            "blocked": null,
            "context_drops": [],
            "suggested_actions": []
        }"#;
        let proposal: ModelResultProposal = serde_json::from_str(raw).unwrap();
        assert_eq!(
            proposal.doc_requests,
            vec![DocTopic::Workflow, DocTopic::Git]
        );
        assert_eq!(
            proposal.scratchpad.as_deref(),
            Some("Verified: parser fixed.")
        );
        let outcome = proposal.outcome.as_ref().expect("outcome proposal");
        assert_eq!(outcome.status, "success");
        assert_eq!(outcome.summary.as_deref(), Some("All checks passed"));
        assert!(outcome.message.is_none());
        assert!(outcome.reason.is_none());
        assert!(proposal.blocked.is_none());

        let json = serde_json::to_string(&proposal).unwrap();
        assert!(json.contains("\"doc_requests\":[\"workflow\",\"git\"]"));
        assert!(json.contains("\"scratchpad\":\"Verified: parser fixed.\""));
        assert!(json.contains("\"status\":\"success\""));
        assert!(!json.contains("\"blocked\""));
        assert_eq!(
            serde_json::from_str::<ModelResultProposal>(&json).unwrap(),
            proposal
        );
    }

    #[test]
    fn test_model_result_proposal_blocked_and_null_tolerance() {
        let raw = r#"{
            "message": "Cannot proceed",
            "changes": [],
            "doc_requests": null,
            "scratchpad": null,
            "outcome": null,
            "blocked": {
                "is_blocked": true,
                "reason": "Missing table",
                "details": "users table absent"
            }
        }"#;
        let proposal: ModelResultProposal = serde_json::from_str(raw).unwrap();
        assert!(proposal.doc_requests.is_empty());
        assert!(proposal.scratchpad.is_none());
        assert!(proposal.outcome.is_none());
        let blocked = proposal.blocked.expect("blocked proposal");
        assert!(blocked.is_blocked);
        assert_eq!(blocked.reason, "Missing table");
        assert_eq!(blocked.details.as_deref(), Some("users table absent"));

        // A minimal payload leaves every extended field at its default, so the
        // schema extension never breaks an existing proposal document.
        let minimal: ModelResultProposal =
            serde_json::from_str(r#"{"message":"ok","changes":[]}"#).unwrap();
        assert!(minimal.doc_requests.is_empty());
        assert!(minimal.scratchpad.is_none());
        assert!(minimal.outcome.is_none());
        assert!(minimal.blocked.is_none());
    }

    #[test]
    fn test_structured_outcome_proposal_skip_and_roundtrip() {
        let proposal = StructuredOutcomeProposal {
            status: "skip".to_string(),
            summary: None,
            message: None,
            reason: Some("Task already satisfied".to_string()),
        };
        assert!(proposal.is_skip());
        assert!(!proposal.is_success());
        assert!(!proposal.is_error());

        let json = serde_json::to_string(&proposal).unwrap();
        assert!(json.contains("\"status\":\"skip\""));
        assert!(json.contains("\"reason\":\"Task already satisfied\""));
        assert!(!json.contains("\"summary\""));
        assert!(!json.contains("\"message\""));

        let de: StructuredOutcomeProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(de, proposal);

        // Also test "skipped" alias variant
        let proposal_skipped = StructuredOutcomeProposal {
            status: "skipped".to_string(),
            summary: None,
            message: None,
            reason: Some("Step not needed".to_string()),
        };
        assert!(proposal_skipped.is_skip());

        // Backwards compatibility: empty/minimal JSON
        let minimal: StructuredOutcomeProposal = serde_json::from_str("{}").unwrap();
        assert_eq!(minimal.status, "");
        assert_eq!(minimal.summary, None);
        assert_eq!(minimal.message, None);
        assert_eq!(minimal.reason, None);
        assert!(!minimal.is_skip());
    }
}
