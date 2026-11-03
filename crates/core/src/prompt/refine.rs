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
//! to it.
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
    PatchStrategy,
};

use crate::edits::stream::discussion::DiscussionStreamFilter;
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
    out.push_str("4. The schema carries the authoritative description of every field and every status value, and accepts no key beyond the declared ones.\n");
    out.push_str("5. Final Content Invariant: every text field carries only the final distilled result. Deliberation, self-questioning, exploratory analysis, and intermediate reasoning are never payload content.\n");
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
        assert!(!protocol.supports(LlmCapability::ToolchainVerify));
    }

    #[test]
    fn test_response_format_derives_from_the_single_schema_definition() {
        let protocol = PlanRefinePromptProtocol::default();
        let format = protocol
            .response_format(&[], PatchStrategy::SearchReplace)
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
}
