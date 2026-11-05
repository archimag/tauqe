//! Architect plan horizon-synthesis prompt protocol.
//!
//! Horizon synthesis is a model-facing contour of its own: the Architect
//! gathers repository evidence, verifies contracts, and answers with either the
//! new unfinished horizon or a blocker report. [`PlanRefinePromptProtocol`] is
//! the typed wire surface of that contour, so a caller asks the protocol for its
//! response schema and its instructions instead of assembling either by hand.
//!
//! The advertised surface is [`CapabilitySet::architect_research`]: the
//! Architect reads context, searches the repository, requests symbol contours,
//! consults the documentation, records working notes, and reports a terminal
//! status. It never edits a file and never runs the toolchain, so neither
//! [`LlmCapability::FileEdits`] nor [`LlmCapability::ToolchainVerify`] belongs
//! to it. A synthesis runs unattended exactly as an isolated step does, so the
//! surface keeps the abort escape hatch: a condition that only the developer
//! can repair on the host is reported through [`LlmCapability::Abort`] instead
//! of an improvised surrogate or a misreported blocker.
//!
//! The provider schema envelope is derived from the single declarative
//! definition in `crate::plan::prompt`, so the envelope and the definition can
//! never disagree about the schema name, its description, or its strictness.
//!
//! The protocol owns the response contract of the contour — the payload shape,
//! the required fields, the status vocabulary, and the formatting rules — while
//! the domain framing of the operation travels in the task fragment of the user
//! message. One protocol therefore serves the initial synthesis, a replanning
//! wave, and a review decomposition without restating any of them here.

use std::borrow::Cow;

use tauqe_protocol::{
    CapabilitySet, ContextAccess, EditProtocolKind, LlmCapability, LlmCommand, LlmOutcome,
    PatchStrategy, ProjectConfig,
};

use crate::edits::stream::discussion::DiscussionStreamFilter;
use crate::edits::stream::json::decode::{first_json_object_str, strip_code_fence};
use crate::edits::stream::EditStreamFilter;
use crate::plan::prompt::{
    parse_plan_refinement_output, plan_refine_response_schema,
    plan_refine_response_schema_definition,
};
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{JsonSchemaDefinition, ResponseFormat};

/// Wire surface of the Architect horizon-synthesis contour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRefinePromptProtocol {
    capabilities: CapabilitySet,
}

impl Default for PlanRefinePromptProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl PlanRefinePromptProtocol {
    /// Creates the protocol bound to the architect research surface.
    pub fn new() -> Self {
        Self {
            capabilities: CapabilitySet::architect_research(),
        }
    }

    /// Returns the strict JSON Schema format of the synthesis payload.
    ///
    /// The envelope is read from the declarative definition of the contour, so
    /// the provider format restates neither the schema name nor its strictness.
    pub fn response_schema_format(&self) -> ResponseFormat {
        let definition = plan_refine_response_schema_definition();
        let name = definition
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("plan_refinement")
            .to_string();
        let description = definition
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let strict = definition
            .get("strict")
            .and_then(serde_json::Value::as_bool);
        let schema = definition
            .get("schema")
            .cloned()
            .unwrap_or_else(plan_refine_response_schema);
        ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name,
                description,
                schema: Cow::Owned(schema),
                strict,
            },
        }
    }

    /// Returns the system instructions of the synthesis contour.
    ///
    /// The instructions carry the response contract of the contour only: the
    /// payload shape, the required fields, the status vocabulary, and the
    /// formatting rules. The domain framing of the synthesis travels in the task
    /// fragment of the user message, so one protocol serves the initial
    /// synthesis, a replanning wave, and a review decomposition without
    /// restating any of them here.
    pub fn instructions(&self) -> String {
        refinement_output_contract()
    }
}

/// Renders the response contract of the horizon-synthesis contour.
///
/// The required field list and the accepted status values are read from the
/// single declarative definition of the contour, so the contract and the schema
/// name the same fields instead of drifting apart.
fn refinement_output_contract() -> String {
    let schema = plan_refine_response_schema();
    let fields = contract_values(&schema, "/required");
    let statuses = contract_values(&schema, "/properties/status/enum");

    let mut out = String::new();
    out.push_str(
        "You are the structured response surface of the Plan Horizon Synthesis contour.\n\n",
    );
    out.push_str("## Output Contract:\n\n");
    out.push_str("1. Reply with exactly one JSON object that satisfies the `plan_refinement` schema. Never wrap the payload in a markdown code fence, never surround it with prose, and never emit a second object.\n");
    out.push_str("2. Every field is mandatory: a field without content still appears in its empty form (`null`, `[]`) instead of being omitted. Required fields: ");
    out.push_str(&fields.join(", "));
    out.push_str(".\n");
    out.push_str("3. `status` decides the meaning of the whole payload and accepts only ");
    out.push_str(&statuses.join(", "));
    out.push_str(".\n");
    out.push_str("   - `success`: `items` carries the complete new unfinished horizon, while `context_requests`, `repo_searches`, and `repo_maps` stay empty.\n");
    out.push_str("   - `needs_context`: the contract you are about to implement is not verified yet, so `items` stays empty, `explanation` names that contract, and the evidence still missing is named by `context_requests`, `repo_searches`, or `repo_maps`.\n");
    out.push_str("   - `blocked`: the horizon cannot be designed, so `reason` states the exact blocker and `items` stays empty.\n");
    out.push_str("4. `abort` reports a host-environment impossibility the harness cannot repair itself, such as a missing system utility or package, a build script awaiting approval, a stopped daemon or database, or an unauthenticated registry. Its idle form is `is_aborted: false`; an active abort (`is_aborted: true`) supersedes any declared `status`, carries the concrete missing capability in `reason` and the exact command the developer must run in `recommendation`, and is never replaced by a `blocked` answer or by an improvised surrogate step. An active abort carries no horizon: `items` stays empty.\n");
    out.push_str("5. The schema carries the authoritative description of every field and every status value, and accepts no key beyond the declared ones.\n");
    out.push_str("6. Final Content Invariant: every text field carries only the final distilled result. Deliberation, self-questioning, exploratory analysis, and intermediate reasoning are never payload content.\n");
    out
}

/// Renders the model-facing list of the values a JSON Schema pointer names.
fn contract_values(schema: &serde_json::Value, pointer: &str) -> Vec<String> {
    schema
        .pointer(pointer)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|value| format!("`{value}`"))
                .collect()
        })
        .unwrap_or_default()
}

/// Reports why a synthesis payload cannot be honored as a horizon.
///
/// Horizon synthesis owns the unfinished horizon alone: the plan payload is the
/// mutation vehicle of the plan dialogue, so a `plan_update` field in a synthesis
/// round lies outside the contract of this contour. The payload is therefore
/// refused through the typed defect channel instead of decoding into a horizon
/// that silently dropped the mutation the model attempted.
///
/// Detection reads the very object the payload parser reads, so a fenced or
/// prose-wrapped payload is inspected identically. A `null` field is the idle
/// representation of an untouched plan and is not a defect.
pub fn unsupported_plan_update_reason(raw_text: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(strip_code_fence(raw_text))
        .ok()
        .or_else(|| {
            first_json_object_str(raw_text).and_then(|object| serde_json::from_str(object).ok())
        })?;
    if value.get("plan_update")?.is_null() {
        return None;
    }
    Some(
        "The payload carries a `plan_update` field, and the plan refinement contract accepts no plan mutation: the horizon is synthesized here, while plan mutations belong to the plan discussion. Regenerate the payload without `plan_update`."
            .to_string(),
    )
}

/// Normalizes the payload of an active abort.
///
/// A report that states no reason still stops the turn, so the deterministic
/// defaults the edit contour uses for a blank field stand in for an empty
/// `reason` and `recommendation` instead of leaving the abort without a cause or
/// without a next step.
fn abort_payload(reason: &str, recommendation: &str) -> (String, String) {
    let reason = reason.trim();
    let reason = if reason.is_empty() {
        "Execution aborted"
    } else {
        reason
    };
    let recommendation = recommendation.trim();
    let recommendation = if recommendation.is_empty() {
        "No recommendation was stated."
    } else {
        recommendation
    };
    (reason.to_string(), recommendation.to_string())
}

impl PromptProtocol for PlanRefinePromptProtocol {
    fn name(&self) -> &'static str {
        "plan_refinement"
    }

    fn kind(&self) -> EditProtocolKind {
        EditProtocolKind::Structured
    }

    fn capabilities(&self) -> &[LlmCapability] {
        self.capabilities.as_slice()
    }

    fn capability_set(&self) -> CapabilitySet {
        self.capabilities.clone()
    }

    fn with_capability_set(&self, capabilities: CapabilitySet) -> Box<dyn PromptProtocol> {
        Box::new(Self { capabilities })
    }

    fn decode(&self, raw_text: &str, _context_paths: &[String]) -> Vec<LlmCommand> {
        // A synthesis round never mutates the plan, so the payload is refused
        // before it is decoded: a horizon that silently dropped the mutation
        // would report a success the developer never asked for.
        if let Some(reason) = unsupported_plan_update_reason(raw_text) {
            return vec![LlmCommand::ProtocolError { message: reason }];
        }
        let output = match parse_plan_refinement_output(raw_text) {
            Ok(output) => output,
            Err(err) => {
                return vec![LlmCommand::ProtocolError {
                    message: format!("Failed to decode the plan refinement payload: {err}"),
                }];
            }
        };

        let mut commands = Vec::new();
        let explanation = output.explanation.trim().to_string();
        if !explanation.is_empty() {
            commands.push(LlmCommand::Text {
                content: explanation.clone(),
            });
        }

        // An active abort supersedes the declared status: the round reports work
        // the harness cannot perform at all, so no horizon, blocker, or research
        // request is decoded beside it. A blank payload keeps the deterministic
        // defaults the edit contour uses, so an abort always states a cause.
        if let Some(abort) = output.abort.as_ref().filter(|abort| abort.is_aborted) {
            let (reason, recommendation) = abort_payload(&abort.reason, &abort.recommendation);
            commands.push(LlmCommand::Abort {
                reason,
                recommendation,
            });
            return commands;
        }

        match output.status.as_str() {
            "needs_context" => {
                // The horizon is not synthesized yet: the round names the evidence
                // it still needs, so the caller widens the context and asks again.
                for path in &output.context_requests {
                    commands.push(LlmCommand::ContextRequest {
                        path: path.clone(),
                        access: ContextAccess::ReadOnly,
                    });
                }
                for search in &output.repo_searches {
                    commands.push(LlmCommand::RepoSearch {
                        path: search.path.clone(),
                        regex: search.regex.clone(),
                    });
                }
                for path in &output.repo_maps {
                    commands.push(LlmCommand::RepoMap { path: path.clone() });
                }
            }
            "blocked" => {
                // The Architect refuses to plan, so the required decision travels
                // as a semantic blocker instead of an improvised horizon.
                let reason = output
                    .reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| explanation.clone());
                let details = if explanation.is_empty() {
                    None
                } else {
                    Some(explanation.clone())
                };
                commands.push(LlmCommand::Blocked { reason, details });
            }
            "success" => {
                let commit_message = if explanation.is_empty() {
                    None
                } else {
                    Some(explanation.clone())
                };
                commands.push(LlmCommand::Outcome {
                    outcome: LlmOutcome::Success { commit_message },
                });
            }
            other => commands.push(LlmCommand::ProtocolError {
                message: format!("Unsupported plan refinement status '{other}'"),
            }),
        }

        commands
    }

    fn system_instructions(
        &self,
        _editable_paths: &[String],
        _has_eager_repo_map: bool,
        _patch_strategy: PatchStrategy,
    ) -> String {
        self.instructions()
    }

    fn response_format(
        &self,
        _editable_paths: &[String],
        _patch_strategy: PatchStrategy,
        _project: Option<&ProjectConfig>,
    ) -> Option<ResponseFormat> {
        Some(self.response_schema_format())
    }

    fn create_stream_filter(
        &self,
        _editable_paths: Vec<String>,
        _repo_root: std::path::PathBuf,
        _staged_contents: std::collections::HashMap<String, String>,
    ) -> Box<dyn EditStreamFilter> {
        Box::new(DiscussionStreamFilter::with_target_field(
            "explanation".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A round that still needs evidence instead of a horizon.
    const NEEDS_CONTEXT: &str = r#"{
        "status": "needs_context",
        "explanation": "The toolchain contract is not verified yet",
        "reason": null,
        "context_requests": ["crates/core/src/toolchain.rs"],
        "repo_searches": [{"path": "crates/core", "regex": "fn run_verification_pipeline"}],
        "repo_maps": ["crates/core/src/plan"],
        "items": []
    }"#;

    #[test]
    fn test_surface_is_the_architect_research_profile() {
        let protocol = PlanRefinePromptProtocol::default();
        assert_eq!(protocol.name(), "plan_refinement");
        assert_eq!(protocol.kind(), EditProtocolKind::Structured);
        assert_eq!(
            protocol.capability_set(),
            CapabilitySet::architect_research()
        );
        assert!(protocol.supports(LlmCapability::Research));
        assert!(protocol.supports(LlmCapability::PlanUpdate));
        assert!(!protocol.supports(LlmCapability::FileEdits));
        // A synthesis runs unattended exactly as an isolated step does, so the
        // surface keeps the escape hatch that demands a host action instead of
        // improvising a surrogate change or misreporting a blocker.
        assert!(protocol.supports(LlmCapability::Abort));
        assert!(!protocol.supports(LlmCapability::ToolchainVerify));
    }

    #[test]
    fn test_response_format_derives_from_the_single_schema_definition() {
        let protocol = PlanRefinePromptProtocol::default();
        let format = protocol
            .response_format(&[], PatchStrategy::SearchReplace, None)
            .expect("the synthesis contour always carries a schema");
        let ResponseFormat::JsonSchema { json_schema } = format else {
            panic!("the synthesis contour is a structured contour");
        };
        let definition = plan_refine_response_schema_definition();
        assert_eq!(definition["name"].as_str(), Some(json_schema.name.as_str()));
        assert_eq!(&definition["schema"], json_schema.schema.as_ref());
        assert_eq!(definition["strict"].as_bool(), Some(true));
    }

    #[test]
    fn test_system_instructions_carry_the_output_contract_only() {
        let protocol = PlanRefinePromptProtocol::default();
        let instructions = protocol.system_instructions(&[], false, PatchStrategy::SearchReplace);

        // The protocol owns the response contract of the contour.
        assert!(instructions.contains("`plan_refinement` schema"));
        assert!(instructions.contains("Final Content Invariant"));
        assert!(instructions.contains("markdown code fence"));
        assert!(instructions.contains("`success`"));
        assert!(instructions.contains("`blocked`"));
        assert!(instructions.contains("`needs_context`"));

        // The domain framing of the synthesis travels in the task fragment of
        // the user message, so the protocol never restates it.
        assert!(!instructions.contains("Wave Horizon Planning"));
        assert!(!instructions.contains("Architect Discovery Protocol"));
        assert!(!instructions.contains("Principal Software Architect"));
    }

    #[test]
    fn test_output_contract_names_every_schema_field_and_status() {
        let instructions = PlanRefinePromptProtocol::default().instructions();
        let schema = plan_refine_response_schema();

        let fields = schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .expect("the schema declares its required fields");
        for field in fields {
            let field = field.as_str().expect("a field name is a string");
            assert!(
                instructions.contains(&format!("`{field}`")),
                "the output contract must name the schema field {field}"
            );
        }

        let statuses = schema
            .pointer("/properties/status/enum")
            .and_then(serde_json::Value::as_array)
            .expect("the schema declares its status values");
        for status in statuses {
            let status = status.as_str().expect("a status value is a string");
            assert!(
                instructions.contains(&format!("`{status}`")),
                "the output contract must name the schema status {status}"
            );
        }
    }

    #[test]
    fn test_decode_needs_context_yields_research_commands() {
        let protocol = PlanRefinePromptProtocol::default();
        let commands = protocol.decode(NEEDS_CONTEXT, &[]);

        assert!(commands.contains(&LlmCommand::Text {
            content: "The toolchain contract is not verified yet".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::ContextRequest {
            path: "crates/core/src/toolchain.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }));
        assert!(commands.contains(&LlmCommand::RepoSearch {
            path: "crates/core".to_string(),
            regex: "fn run_verification_pipeline".to_string(),
        }));
        assert!(commands.contains(&LlmCommand::RepoMap {
            path: "crates/core/src/plan".to_string(),
        }));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Outcome { .. })));
    }

    #[test]
    fn test_decode_blocked_yields_a_semantic_blocker() {
        let protocol = PlanRefinePromptProtocol::default();
        let blocked = r#"{
            "status": "blocked",
            "explanation": "The signature cannot be satisfied",
            "reason": "Trait Display is not implemented for the chosen type",
            "context_requests": [],
            "repo_searches": [],
            "repo_maps": [],
            "items": []
        }"#;
        let commands = protocol.decode(blocked, &[]);

        assert!(commands.contains(&LlmCommand::Blocked {
            reason: "Trait Display is not implemented for the chosen type".to_string(),
            details: Some("The signature cannot be satisfied".to_string()),
        }));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Outcome { .. })));
    }

    #[test]
    fn test_decode_success_yields_a_terminal_outcome() {
        let protocol = PlanRefinePromptProtocol::default();
        let success = r#"{
            "status": "success",
            "explanation": "Decomposed into three verified steps",
            "reason": null,
            "context_requests": [],
            "repo_searches": [],
            "repo_maps": [],
            "items": []
        }"#;

        assert_eq!(
            protocol.decode(success, &[]),
            vec![
                LlmCommand::Text {
                    content: "Decomposed into three verified steps".to_string(),
                },
                LlmCommand::Outcome {
                    outcome: LlmOutcome::Success {
                        commit_message: Some("Decomposed into three verified steps".to_string()),
                    },
                },
            ]
        );
    }

    #[test]
    fn test_decode_refuses_a_payload_that_mutates_the_plan() {
        let protocol = PlanRefinePromptProtocol::default();
        let mutating = r#"{
            "status": "success",
            "explanation": "Horizon synthesized",
            "reason": null,
            "plan_update": {"action": "update", "id": "p-1", "title": "P1"},
            "context_requests": [],
            "repo_searches": [],
            "repo_maps": [],
            "items": []
        }"#;
        let commands = protocol.decode(mutating, &[]);
        assert_eq!(commands.len(), 1, "{commands:?}");
        match &commands[0] {
            LlmCommand::ProtocolError { message } => {
                assert!(message.contains("plan_update"), "{message}");
            }
            other => panic!("expected the refusal, got {other:?}"),
        }

        // The idle representation of an untouched plan is not a defect, so the
        // very same round decodes into its horizon.
        let idle = mutating.replace(
            r#""plan_update": {"action": "update", "id": "p-1", "title": "P1"}"#,
            r#""plan_update": null"#,
        );
        assert!(idle.contains(r#""plan_update": null"#));
        assert!(!protocol
            .decode(&idle, &[])
            .iter()
            .any(|command| matches!(command, LlmCommand::ProtocolError { .. })));
    }

    #[test]
    fn test_decode_rejects_an_unparsable_round() {
        let protocol = PlanRefinePromptProtocol::default();
        let commands = protocol.decode("not a plan refinement payload", &[]);
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            commands.first(),
            Some(LlmCommand::ProtocolError { .. })
        ));
    }

    #[test]
    fn test_no_turn_marker_and_streamed_explanation() {
        let protocol = PlanRefinePromptProtocol::default();
        // The contour is structured, so it mints no marker-bound variant and
        // leaves the value unset on every level that would carry one.
        assert!(protocol.turn_marker().is_none());
        assert!(protocol.with_turn_marker("M12345").is_none());

        let mut filter = protocol.create_stream_filter(
            vec![],
            std::path::PathBuf::from("/tmp"),
            std::collections::HashMap::new(),
        );
        let streamed: String = filter
            .push_chunk("{\"explanation\":\"Decomposed\"")
            .into_iter()
            .filter_map(|event| match event {
                crate::providers::StreamEvent::TextDelta(delta) => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "Decomposed");
    }
    /// A round that reports a host-environment impossibility instead of a horizon.
    const ABORTED: &str = r#"{
        "status": "success",
        "explanation": "The native build needs an approval only the developer can give",
        "reason": null,
        "abort": {
            "is_aborted": true,
            "reason": "  Native build approval required  ",
            "recommendation": "  pnpm approve-builds  "
        },
        "context_requests": ["crates/core/src/toolchain.rs"],
        "repo_searches": [],
        "repo_maps": [],
        "items": []
    }"#;

    #[test]
    fn test_decode_an_active_abort_supersedes_the_declared_status() {
        let protocol = PlanRefinePromptProtocol::default();
        let commands = protocol.decode(ABORTED, &[]);

        // The escape hatch reaches the harness as one command, with the payload
        // trimmed exactly as the edit contour trims it.
        assert!(commands.contains(&LlmCommand::Abort {
            reason: "Native build approval required".to_string(),
            recommendation: "pnpm approve-builds".to_string(),
        }));

        // The declared status is superseded: no terminal outcome, no blocker, and
        // no research request is decoded beside the abort.
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Outcome { .. })));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, LlmCommand::Blocked { .. })));
        assert!(!commands.iter().any(|command| matches!(
            command,
            LlmCommand::ContextRequest { .. }
                | LlmCommand::RepoSearch { .. }
                | LlmCommand::RepoMap { .. }
        )));

        // The explanation still reaches the developer as conversational text.
        assert!(commands.contains(&LlmCommand::Text {
            content: "The native build needs an approval only the developer can give".to_string(),
        }));
    }

    #[test]
    fn test_decode_an_active_abort_states_a_deterministic_payload() {
        let protocol = PlanRefinePromptProtocol::default();
        let blank = r#"{
            "status": "success",
            "explanation": "",
            "reason": null,
            "abort": {"is_aborted": true, "reason": "   ", "recommendation": " "},
            "context_requests": [],
            "repo_searches": [],
            "repo_maps": [],
            "items": []
        }"#;
        assert_eq!(
            protocol.decode(blank, &[]),
            vec![LlmCommand::Abort {
                reason: "Execution aborted".to_string(),
                recommendation: "No recommendation was stated.".to_string(),
            }]
        );
    }

    #[test]
    fn test_output_contract_states_the_abort_precedence() {
        let instructions = PlanRefinePromptProtocol::default().instructions();

        // The escape hatch is declared beside the status vocabulary, and the
        // contract names its idle form, its precedence, and the prohibition on
        // downgrading it into a plan-invariant blocker.
        assert!(instructions.contains("`abort`"));
        assert!(instructions.contains("`is_aborted: false`"));
        assert!(instructions.contains("supersedes any declared `status`"));
        assert!(instructions.contains("`items` stays empty"));
        assert!(instructions.contains("never replaced by a `blocked` answer"));
    }
}
