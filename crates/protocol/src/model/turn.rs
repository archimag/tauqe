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

/// One member of the ordered queue a batch turn advances through.
///
/// A queue is described by data, not by the interface: a plan step lists the
/// steps it executes, a review group step lists the sub-steps of the group
/// subplan, and a review step lists the findings of the session. `id` is the
/// identifier the owning subsystem already uses — a plan or sub-step
/// identifier, or a finding number rendered as text — so one sliding-window
/// renderer serves every kind of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnWorkItem {
    pub id: String,
    pub title: String,
}

/// Ordered queue of work a batch turn advances through, addressed by position.
///
/// `active_index` addresses `items`: the member the turn currently executes.
/// [`Self::active`] performs the lookup, so a turn whose index falls outside
/// the list reports no active member instead of forcing every client to
/// bounds-check the pair itself. An empty queue means the turn is not a member
/// of a batch.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TurnWorkQueue {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<TurnWorkItem>,
    #[serde(default)]
    pub active_index: usize,
}

impl TurnWorkQueue {
    /// The member the turn currently executes, or `None` outside the list.
    pub fn active(&self) -> Option<&TurnWorkItem> {
        self.items.get(self.active_index)
    }

    /// One-based position of the active member together with the queue length.
    pub fn position(&self) -> Option<(usize, usize)> {
        self.active()
            .map(|_| (self.active_index + 1, self.items.len()))
    }
}

/// Lifecycle phase of a review run that executes no plan step of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewWorkPhase {
    /// The audit that produces the findings of a session or one of its groups.
    Audit,
    /// The just-in-time decomposition of an architectural group into a subplan.
    Decomposition,
    /// The cross-group classification of the produced findings.
    PostAnalysis,
}

/// Work the model executes in a turn, declared by whoever starts the turn.
///
/// A turn never infers its own task: the harness states it here, so the
/// interface renders the running work — its kind, the entity that owns it, the
/// active member, and its position in the batch queue — from typed fields
/// instead of reconstructing them from the stored plans, reviews, and group
/// lists. The variant is the work kind, so an interactive answer can never be
/// mistaken for a plan step and no kind is decoded from a title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnWorkContext {
    /// Interactive dialogue turn that answers the developer directly.
    Interactive,
    /// One step of a local plan.
    PlanStep {
        plan_id: String,
        plan_title: String,
        queue: TurnWorkQueue,
    },
    /// One sub-step of an architectural review group subplan.
    ReviewGroupStep {
        review_id: String,
        review_title: String,
        group_id: String,
        group_title: String,
        queue: TurnWorkQueue,
    },
    /// One finding of a review session executed on its own.
    ReviewStep {
        review_id: String,
        review_title: String,
        queue: TurnWorkQueue,
    },
    /// A review lifecycle phase that executes no plan step.
    ReviewPhase {
        review_id: String,
        review_title: String,
        phase: ReviewWorkPhase,
        queue: TurnWorkQueue,
    },
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
    /// Work the turn executes, declared by whoever started it.
    ///
    /// Absent from a producer that predates the contract, so a client falls
    /// back to the coordinates it already receives instead of guessing a kind
    /// it was never told. A declared [`TurnWorkContext::Interactive`] states
    /// that the turn answers the developer rather than executing stored work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_context: Option<TurnWorkContext>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_batch: bool,
}

/// Notification that the running role escalated to a stronger model tier.
///
/// Escalation happens when deterministic verification keeps rejecting the same
/// change set: the harness promotes the turn to the next coder tier entry of
/// the same role instead of switching the role. Clients use this event to
/// update the running model label and the reasoning-effort badge without
/// waiting for the turn to finish. Every model coordinate mirrors the
/// [`ModelStartedEvent`] fields, and `tier` is the zero-based index of the
/// tier the turn has entered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEscalatedEvent {
    pub operation_id: String,
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ModelRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    pub tier: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeltaEvent {
    pub operation_id: String,
    pub delta: String,
}

/// Telemetry of a pending provider reconnection.
///
/// Emitted once per second of the backoff pause so the client can render a
/// live countdown next to the interruption hint. `attempt` is the attempt that
/// already failed (`1` for the first retry), `max_attempts` bounds the ladder,
/// and `delay_secs` is the time still remaining before the next attempt starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConnectionRetryingEvent {
    pub operation_id: String,
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_secs: u64,
    pub reason: String,
}

/// Instruction to discard the partially streamed answer.
///
/// Sent when a connection broke after some deltas had already reached the
/// client. Consumers drop everything accumulated for this turn before the
/// retried stream restarts, so a restarted answer is never spliced onto the
/// fragment of the aborted one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStreamResetEvent {
    pub operation_id: String,
    pub reason: String,
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

/// Unconditional abort declared through a structured proposal.
///
/// Mirrors the `<abort>` protocol tag: `is_abort` is the explicit flag the
/// model sets when external environment or host action is required,
/// `reason` names the blocker, and `recommendation` provides actionable guidance for the developer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StructuredAbortProposal {
    #[serde(default)]
    pub is_abort: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub recommendation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelResultProposal {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub changes: Vec<StructuredChangeProposal>,
    #[serde(
        default,
        deserialize_with = "deserialize_context_request_specs",
        skip_serializing_if = "Vec::is_empty"
    )]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort: Option<StructuredAbortProposal>,
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
///
/// A JSON `null` deserializes to an empty list, so a model that emits an
/// explicit null never aborts the proposal parse.
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

    let specs = Option::<Vec<ContextRequestSpec>>::deserialize(deserializer)?;
    Ok(specs
        .unwrap_or_default()
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
    fn test_model_escalated_event_serde() {
        let event = ModelEscalatedEvent {
            operation_id: "op-1".to_string(),
            model: ModelRef::openrouter("anthropic/claude-3.7-sonnet"),
            role: Some(ModelRole::Coder),
            alias: Some("deep".to_string()),
            effort: Some(ReasoningEffort::High),
            tier: 2,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"model\":{\"provider\":\"openrouter\""));
        assert!(json.contains("\"role\":\"coder\""));
        assert!(json.contains("\"alias\":\"deep\""));
        assert!(json.contains("\"tier\":2"));

        let de: ModelEscalatedEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(de.operation_id, "op-1");
        assert_eq!(
            de.model,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );
        assert!(matches!(de.role, Some(ModelRole::Coder)));
        assert_eq!(de.alias.as_deref(), Some("deep"));
        assert!(matches!(de.effort, Some(ReasoningEffort::High)));
        assert_eq!(de.tier, 2);

        // An escalation without an alias or an explicit effort stays decodable:
        // both coordinates are optional and absent from the wire form.
        let minimal: ModelEscalatedEvent = serde_json::from_str(
            r#"{"operation_id":"op-2","model":{"provider":"openrouter","name":"m"},"tier":0}"#,
        )
        .unwrap();
        assert_eq!(minimal.operation_id, "op-2");
        assert!(minimal.role.is_none());
        assert!(minimal.alias.is_none());
        assert!(minimal.effort.is_none());
        assert_eq!(minimal.tier, 0);
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
        assert_eq!(
            serde_json::to_string(&CancelMode::Soft).unwrap(),
            "\"soft\""
        );
        assert_eq!(
            serde_json::to_string(&CancelMode::Hard).unwrap(),
            "\"hard\""
        );
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
            "context_requests": null,
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
        assert!(proposal.context_requests.is_empty());
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
        assert!(minimal.abort.is_none());
    }

    #[test]
    fn test_model_result_proposal_abort_and_null_tolerance() {
        let raw = r#"{
            "message": "Need developer action",
            "changes": [],
            "abort": {
                "is_abort": true,
                "reason": "Approval required",
                "recommendation": "Run pnpm approve-builds"
            }
        }"#;
        let proposal: ModelResultProposal = serde_json::from_str(raw).unwrap();
        let abort = proposal.abort.as_ref().expect("abort proposal");
        assert!(abort.is_abort);
        assert_eq!(abort.reason, "Approval required");
        assert_eq!(abort.recommendation, "Run pnpm approve-builds");

        let json = serde_json::to_string(&proposal).unwrap();
        assert!(json.contains("\"abort\""));
        assert!(json.contains("\"is_abort\":true"));
        assert!(json.contains("\"reason\":\"Approval required\""));
        assert!(json.contains("\"recommendation\":\"Run pnpm approve-builds\""));
        assert_eq!(
            serde_json::from_str::<ModelResultProposal>(&json).unwrap(),
            proposal
        );
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

    #[test]
    fn test_model_result_proposal_omits_empty_discovery_fields() {
        // An empty discovery payload is omitted instead of serialized as an
        // empty array, matching every sibling field of the proposal.
        let proposal = ModelResultProposal {
            message: "No context needed".to_string(),
            ..ModelResultProposal::default()
        };
        let json = serde_json::to_string(&proposal).unwrap();
        assert!(!json.contains("context_requests"));
        assert!(!json.contains("repo_searches"));

        // An omitted field and an explicit null stay equivalent on decode.
        let decoded: ModelResultProposal =
            serde_json::from_str(r#"{"message":"x","context_requests":null}"#).unwrap();
        assert!(decoded.context_requests.is_empty());
    }
    /// The two-step queue every work-context test addresses.
    fn two_step_queue() -> TurnWorkQueue {
        TurnWorkQueue {
            items: vec![
                TurnWorkItem {
                    id: "1".to_string(),
                    title: "Declare the work contract".to_string(),
                },
                TurnWorkItem {
                    id: "2".to_string(),
                    title: "Wire the producers".to_string(),
                },
            ],
            active_index: 1,
        }
    }

    #[test]
    fn test_turn_work_queue_reports_the_active_member() {
        let queue = two_step_queue();
        assert_eq!(queue.active().map(|item| item.id.as_str()), Some("2"));
        assert_eq!(
            queue.active().map(|item| item.title.as_str()),
            Some("Wire the producers")
        );
        // The position clients render as `[i/N]` is one-based.
        assert_eq!(queue.position(), Some((2, 2)));

        // An index outside the list reports no active member instead of forcing
        // every client to bounds-check the pair itself.
        let beyond = TurnWorkQueue {
            items: two_step_queue().items,
            active_index: 5,
        };
        assert!(beyond.active().is_none());
        assert!(beyond.position().is_none());

        // A turn that is not a batch member carries an empty queue.
        let idle = TurnWorkQueue::default();
        assert!(idle.active().is_none());
        assert!(idle.position().is_none());
    }

    #[test]
    fn test_turn_work_context_plan_step_serde() {
        let context = TurnWorkContext::PlanStep {
            plan_id: "plan-1".to_string(),
            plan_title: "Universal work marker".to_string(),
            queue: two_step_queue(),
        };
        let json = serde_json::to_string(&context).unwrap();
        assert!(json.contains("\"kind\":\"plan_step\""));
        assert!(json.contains("\"plan_id\":\"plan-1\""));
        assert!(json.contains("\"active_index\":1"));
        assert!(json.contains("\"items\":[{\"id\":\"1\""));

        let decoded: TurnWorkContext = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, context);
    }

    #[test]
    fn test_turn_work_context_review_variants_serde() {
        let finding = TurnWorkItem {
            id: "7".to_string(),
            title: "Shared transaction boundary".to_string(),
        };
        let finding_queue = TurnWorkQueue {
            items: vec![finding],
            active_index: 0,
        };

        let step = TurnWorkContext::ReviewStep {
            review_id: "rev-1".to_string(),
            review_title: "Multi-level audit".to_string(),
            queue: finding_queue.clone(),
        };
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"kind\":\"review_step\""));
        assert_eq!(
            serde_json::from_str::<TurnWorkContext>(&json).unwrap(),
            step
        );

        let group_step = TurnWorkContext::ReviewGroupStep {
            review_id: "rev-1".to_string(),
            review_title: "Multi-level audit".to_string(),
            group_id: "arch-1".to_string(),
            group_title: "Shared commit boundary".to_string(),
            queue: finding_queue,
        };
        let json = serde_json::to_string(&group_step).unwrap();
        assert!(json.contains("\"kind\":\"review_group_step\""));
        assert!(json.contains("\"group_id\":\"arch-1\""));
        assert_eq!(
            serde_json::from_str::<TurnWorkContext>(&json).unwrap(),
            group_step
        );
    }

    #[test]
    fn test_turn_work_context_phase_and_interactive_serde() {
        let interactive = TurnWorkContext::Interactive;
        let json = serde_json::to_string(&interactive).unwrap();
        assert_eq!(json, "{\"kind\":\"interactive\"}");
        assert_eq!(
            serde_json::from_str::<TurnWorkContext>(&json).unwrap(),
            interactive
        );

        let phase = TurnWorkContext::ReviewPhase {
            review_id: "rev-1".to_string(),
            review_title: "Multi-level audit".to_string(),
            phase: ReviewWorkPhase::PostAnalysis,
            queue: TurnWorkQueue::default(),
        };
        let json = serde_json::to_string(&phase).unwrap();
        assert!(json.contains("\"kind\":\"review_phase\""));
        assert!(json.contains("\"phase\":\"post_analysis\""));
        // A phase that executes no batch member omits the empty item list
        // instead of sending `[]`.
        assert!(!json.contains("\"items\""));
        assert_eq!(
            serde_json::from_str::<TurnWorkContext>(&json).unwrap(),
            phase
        );

        // Every phase keeps a stable wire spelling of its own.
        for (value, expected) in [
            (ReviewWorkPhase::Audit, "\"audit\""),
            (ReviewWorkPhase::Decomposition, "\"decomposition\""),
            (ReviewWorkPhase::PostAnalysis, "\"post_analysis\""),
        ] {
            assert_eq!(serde_json::to_string(&value).unwrap(), expected);
        }
    }

    #[test]
    fn test_model_started_event_carries_an_optional_work_context() {
        // A producer that predates the work contract sends no `work_context`:
        // the event still decodes, and the absence stays distinguishable from a
        // declared interactive turn.
        let legacy: ModelStartedEvent = serde_json::from_str(
            r#"{"operation_id":"op-1","model":{"provider":"openrouter","name":"m"}}"#,
        )
        .unwrap();
        assert!(legacy.work_context.is_none());
        assert!(legacy.plan_context.is_none());
        let json = serde_json::to_string(&legacy).unwrap();
        assert!(!json.contains("work_context"));
        assert!(!json.contains("plan_context"));

        let event = ModelStartedEvent {
            operation_id: "op-2".to_string(),
            model: ModelRef::openrouter("coder/model"),
            role: Some(ModelRole::Coder),
            alias: None,
            effort: Some(ReasoningEffort::High),
            plan_context: Some(ModelTurnPlanContext {
                plan_id: "plan-1".to_string(),
                step_id: "1".to_string(),
                batch_step_ids: vec!["1".to_string(), "2".to_string()],
            }),
            work_context: Some(TurnWorkContext::PlanStep {
                plan_id: "plan-1".to_string(),
                plan_title: "Universal work marker".to_string(),
                queue: two_step_queue(),
            }),
            is_batch: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"work_context\":{\"kind\":\"plan_step\""));
        // The plan stepper keeps receiving the coordinates it always read, so
        // the new field declares the running work instead of replacing them.
        assert!(json.contains("\"plan_context\""));
        assert!(json.contains("\"is_batch\":true"));

        let decoded: ModelStartedEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.operation_id, "op-2");
        assert_eq!(decoded.work_context, event.work_context);
        assert_eq!(decoded.plan_context, event.plan_context);
    }
}
