use serde::{Deserialize, Serialize};

use crate::context::ContextAccess;
use crate::discussion::DiscussionPlanUpdate;
use crate::edit::EditOperation;

/// Declarative set of interaction capabilities a prompt protocol may enable.
///
/// Capabilities decouple the concrete protocol syntax (XML markers, JSON schema)
/// from the core business logic. A protocol advertises the capabilities it
/// supports, and its decoder produces only the corresponding [`LlmCommand`]
/// variants. This keeps the execution pipeline independent of any single wire
/// format and makes protocols interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmCapability {
    /// Free-form conversational Markdown text.
    TextMessage,
    /// File creation, replacement, overwrite, deletion, and move operations.
    FileEdits,
    /// Requests to add or drop files from the working context.
    Discovery,
    /// Requests for system documentation by topic.
    DocRequest,
    /// Requests to run deterministic toolchain verification.
    ToolchainVerify,
    /// Creation of a new project plan.
    PlanCreation,
    /// Modification of an existing project plan.
    PlanUpdate,
    /// Ephemeral working-memory notes for multi-round reasoning.
    Scratchpad,
    /// Explicit terminal outcome (`success` or `error`) of a turn.
    Outcome,
    /// Explicit semantic blocker emitted when preconditions or assumptions are violated.
    Blocked,
    /// Detected primary natural language of the user prompt.
    UserLanguage,
}

/// Normalized intent decoded from raw model output.
///
/// The protocol-specific syntax (XML tags, JSON schema) is translated into this
/// uniform command list so the core pipeline operates purely on semantics. Each
/// command corresponds to an enabled [`LlmCapability`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmCommand {
    /// Conversational text emitted to the user interface.
    Text { content: String },
    /// A single file edit operation.
    Edit { operation: EditOperation },
    /// A request to add a file to the context at a given access level.
    ContextRequest { path: String, access: ContextAccess },
    /// A request for system documentation by topic.
    DocRequest { topic: String },
    /// A request to run deterministic toolchain verification.
    Verify {
        target: VerifyTarget,
        on_success: VerifyOnSuccess,
    },
    /// A plan creation or modification payload.
    Plan { update: DiscussionPlanUpdate },
    /// Ephemeral working-memory notes.
    Scratchpad { notes: String },
    /// Explicit terminal outcome of a turn.
    Outcome { outcome: LlmOutcome },
    /// Explicit semantic blocker emitted when plan invariants or preconditions fail.
    Blocked {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<String>,
    },
    /// Detected primary natural language of the user prompt.
    UserLanguage { language: String },
}

/// Deterministic toolchain verification target requested by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyTarget {
    /// Run every configured verification command.
    All,
    /// Compile and type-check only.
    Check,
    /// Run the test suite only.
    Test,
    /// Run the linter only.
    Clippy,
}

/// Desired behavior when toolchain verification succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOnSuccess {
    /// Report only a concise confirmation.
    Silent,
    /// Surface the full command output.
    Report,
}

/// Explicit terminal outcome emitted by the model at the end of a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LlmOutcome {
    /// The task completed successfully, with an optional summary.
    Success { summary: Option<String> },
    /// The task is blocked, with an explanatory message.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_capability_serde_roundtrip() {
        let cases = [
            (LlmCapability::TextMessage, "\"text_message\""),
            (LlmCapability::FileEdits, "\"file_edits\""),
            (LlmCapability::Discovery, "\"discovery\""),
            (LlmCapability::DocRequest, "\"doc_request\""),
            (LlmCapability::ToolchainVerify, "\"toolchain_verify\""),
            (LlmCapability::PlanCreation, "\"plan_creation\""),
            (LlmCapability::PlanUpdate, "\"plan_update\""),
            (LlmCapability::Scratchpad, "\"scratchpad\""),
            (LlmCapability::Outcome, "\"outcome\""),
            (LlmCapability::Blocked, "\"blocked\""),
            (LlmCapability::UserLanguage, "\"user_language\""),
        ];

        for (capability, expected) in cases {
            assert_eq!(serde_json::to_string(&capability).unwrap(), expected);
            let decoded: LlmCapability = serde_json::from_str(expected).unwrap();
            assert_eq!(decoded, capability);
        }
    }

    #[test]
    fn test_llm_command_text_and_edit_serde_roundtrip() {
        let text = LlmCommand::Text {
            content: "Refactored the module".to_string(),
        };
        let json = serde_json::to_string(&text).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), text);

        let edit = LlmCommand::Edit {
            operation: EditOperation::Replace {
                path: "src/lib.rs".to_string(),
                old_text: "let a = 1;".to_string(),
                new_text: "let a = 2;".to_string(),
            },
        };
        let json = serde_json::to_string(&edit).unwrap();
        assert!(json.contains("\"type\":\"edit\""));
        assert!(json.contains("\"operation\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), edit);
    }

    #[test]
    fn test_llm_command_discovery_serde_roundtrip() {
        let request = LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::Editable,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"context_request\""));
        assert!(json.contains("\"access\":\"editable\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), request);

        let doc = LlmCommand::DocRequest {
            topic: "workflow".to_string(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        assert!(json.contains("\"type\":\"doc_request\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), doc);
    }

    #[test]
    fn test_llm_command_verify_serde_roundtrip() {
        let verify = LlmCommand::Verify {
            target: VerifyTarget::Clippy,
            on_success: VerifyOnSuccess::Silent,
        };
        let json = serde_json::to_string(&verify).unwrap();
        assert!(json.contains("\"type\":\"verify\""));
        assert!(json.contains("\"target\":\"clippy\""));
        assert!(json.contains("\"on_success\":\"silent\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), verify);

        assert_eq!(serde_json::to_string(&VerifyTarget::All).unwrap(), "\"all\"");
        assert_eq!(
            serde_json::to_string(&VerifyOnSuccess::Report).unwrap(),
            "\"report\""
        );
    }

    #[test]
    fn test_llm_command_plan_scratchpad_language_serde_roundtrip() {
        let plan = LlmCommand::Plan {
            update: DiscussionPlanUpdate::default(),
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"type\":\"plan\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), plan);

        let scratchpad = LlmCommand::Scratchpad {
            notes: "Verified: parser fixed.".to_string(),
        };
        let json = serde_json::to_string(&scratchpad).unwrap();
        assert!(json.contains("\"type\":\"scratchpad\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), scratchpad);

        let language = LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        };
        let json = serde_json::to_string(&language).unwrap();
        assert!(json.contains("\"type\":\"user_language\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), language);
    }

    #[test]
    fn test_llm_command_outcome_serde_roundtrip() {
        let success = LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                summary: Some("All checks passed".to_string()),
            },
        };
        let json = serde_json::to_string(&success).unwrap();
        assert!(json.contains("\"type\":\"outcome\""));
        assert!(json.contains("\"status\":\"success\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), success);

        let error = LlmCommand::Outcome {
            outcome: LlmOutcome::Error {
                message: "Missing dependency".to_string(),
            },
        };
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("\"status\":\"error\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), error);
    }

    #[test]
    fn test_llm_command_blocked_serde_roundtrip() {
        let blocked = LlmCommand::Blocked {
            reason: "Precondition failed".to_string(),
            details: Some("Missing table in db".to_string()),
        };
        let json = serde_json::to_string(&blocked).unwrap();
        assert!(json.contains("\"type\":\"blocked\""));
        assert!(json.contains("\"reason\":\"Precondition failed\""));
        assert!(json.contains("\"details\":\"Missing table in db\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), blocked);
    }
}
