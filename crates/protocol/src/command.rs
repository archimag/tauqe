//! Normalized model-output command vocabulary.
//!
//! The protocol-specific syntax (XML tags, JSON schema) is translated into the
//! uniform [`LlmCommand`] list declared here, so the core pipeline operates
//! purely on semantics. Two self-contained vocabularies live in their own
//! submodules and are re-exported from this facade, so every existing
//! `tauqe_protocol::command::Item` path keeps resolving unchanged:
//!
//! - [`capability`] declares the capability classification model shared by
//!   every prompt protocol.
//! - [`topic`] declares the documentation topic vocabulary.

use serde::{Deserialize, Serialize};

use crate::context::ContextAccess;
use crate::discussion::{DiscussionPlanAction, DiscussionPlanUpdate};
use crate::edit::EditOperation;

pub mod capability;
pub mod topic;

pub use capability::{
    CapabilityDelivery, CapabilityPhase, CapabilityProfile, CapabilitySet, LlmCapability,
};
pub use topic::DocTopic;

/// Normalized intent decoded from raw model output.
///
/// The protocol-specific syntax (XML tags, JSON schema) is translated into this
/// uniform command list so the core pipeline operates purely on semantics. Each
/// command corresponds to an enabled [`LlmCapability`].
///
/// `Eq` is deliberately absent: a custom tool invocation carries arbitrary JSON
/// arguments, and a JSON number may hold a float, so the command list is
/// comparable only through `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmCommand {
    /// Conversational text emitted to the user interface.
    Text { content: String },
    /// A single file edit operation.
    ///
    /// [`EditOperation`] confines every path it carries at the serde boundary,
    /// so a decoded edit can never address a location outside the repository.
    Edit { operation: EditOperation },
    /// A request to add a file to the context at a given access level.
    ///
    /// The path follows the same wire rules as an [`EditOperation`] path: the
    /// serde boundary normalizes a benign spelling and rejects a spelling that
    /// escapes the repository, because a typed variant cannot silently drop
    /// itself. The engine still confines the path against the repository root.
    ContextRequest {
        #[serde(deserialize_with = "crate::edit::proposal::deserialize_repo_path")]
        path: String,
        access: ContextAccess,
    },
    /// A search request across repository files using Rust/ripgrep regex.
    ///
    /// `path` is repository-relative and confines the search to that subtree;
    /// an empty path names the whole repository. The serde boundary applies the
    /// crate's shared repository-path rules, so a benign spelling such as
    /// `./src` normalizes to `src` and a spelling that escapes the repository
    /// surfaces as a decode failure naming it instead of reaching the engine: a
    /// typed variant cannot silently drop itself. A programmatic producer hands
    /// over an already-normalized path, and the engine still confines the path
    /// at execution, exactly as
    /// [`normalize_repo_path`](crate::edit::normalize_repo_path) documents.
    RepoSearch {
        #[serde(deserialize_with = "crate::discussion::normalize::deserialize_repo_map_path")]
        path: String,
        regex: String,
    },
    /// A request for the compact repository directory tree.
    RepoDirs,
    /// A request for the targeted symbol map of one subdirectory.
    ///
    /// The path is a repository-relative prefix matching whole path components;
    /// an empty path names the whole repository. The serde boundary applies the
    /// same rules as the search request: a benign spelling normalizes, and an
    /// escaping one is a decode failure naming it.
    RepoMap {
        #[serde(deserialize_with = "crate::discussion::normalize::deserialize_repo_map_path")]
        path: String,
    },
    /// A request for the tracked-file listing of one directory.
    ///
    /// The path is a repository-relative directory prefix matching whole path
    /// components; an empty path names the repository root. `recursive` widens
    /// the listing from the direct children to every tracked file underneath,
    /// keeping the default request shallow. The serde boundary applies the same
    /// rules as the search request, so a listing can never address a location
    /// outside the repository.
    DirList {
        #[serde(deserialize_with = "crate::discussion::normalize::deserialize_repo_map_path")]
        path: String,
        recursive: bool,
    },
    /// A request for harness system documentation by topic.
    ///
    /// The command is canonically spelled `tauqe_doc_request`, and the
    /// pre-canonical `doc_request` stays a declared alias, so a payload written
    /// before the rename keeps decoding identically.
    ///
    /// The topic may be omitted: a payload without one decodes to
    /// [`DocTopic::All`], so the documented missing-topic fallback holds on
    /// this boundary as well.
    #[serde(rename = "tauqe_doc_request", alias = "doc_request")]
    DocRequest {
        #[serde(default)]
        topic: DocTopic,
    },
    /// A request for one external document declared by the project.
    ///
    /// The identifier names an entry of `[[outer_docs]]`, never a path: where
    /// the materialized copy is cached stays an implementation detail of the
    /// harness, so a request addresses only material the project itself
    /// declared. A document that publishes a structured index answers with the
    /// chapters it exposes, and the material it delivers enters the context as
    /// read-only reference.
    OuterDocRequest { id: String },
    /// A request to run deterministic toolchain verification.
    Verify {
        target: VerifyTarget,
        on_success: VerifyOnSuccess,
    },
    /// A plan creation or modification payload.
    Plan { update: DiscussionPlanUpdate },
    /// A decision recorded for one finding of a review session.
    ///
    /// The decision is authorized by the declared regime of the turn: only a
    /// review discussion carries the capability, so the harness applies it
    /// against the session the turn names and never against whichever session
    /// happens to be active.
    ReviewUpdate {
        update: crate::review::ReviewUpdateProposal,
    },
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
    /// Explicit unconditional abort emitted when external host environment actions or developer intervention is required.
    Abort {
        reason: String,
        recommendation: String,
    },
    /// Detected primary natural language of the user prompt.
    UserLanguage { language: String },
    /// A typed invocation of one project-declared custom tool.
    ///
    /// `name` addresses a command registered in the project configuration, and
    /// `args` carries the typed argument object the command declares. The
    /// harness validates both against the declared allowlist, so an invocation
    /// can never widen into an arbitrary shell command.
    CustomTool {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Protocol syntax or validation error encountered while parsing model output.
    ProtocolError { message: String },
}

impl LlmCommand {
    /// Capability that this command realizes, or `None` when the command is a
    /// protocol diagnostic rather than a realized capability.
    pub fn capability(&self) -> Option<LlmCapability> {
        match self {
            Self::Text { .. } => Some(LlmCapability::TextMessage),
            Self::Edit { .. } => Some(LlmCapability::FileEdits),
            Self::ContextRequest { .. }
            | Self::RepoSearch { .. }
            | Self::RepoDirs
            | Self::RepoMap { .. }
            | Self::DirList { .. } => Some(LlmCapability::Research),
            Self::DocRequest { .. } => Some(LlmCapability::DocRequest),
            Self::OuterDocRequest { .. } => Some(LlmCapability::OuterDocRequest),
            Self::Verify { .. } => Some(LlmCapability::ToolchainVerify),
            Self::Plan { update } => Some(match update.action {
                DiscussionPlanAction::Save => LlmCapability::PlanCreation,
                DiscussionPlanAction::Update | DiscussionPlanAction::Delete => {
                    LlmCapability::PlanUpdate
                }
            }),
            Self::Scratchpad { .. } => Some(LlmCapability::Scratchpad),
            Self::Outcome { .. } => Some(LlmCapability::Outcome),
            Self::Blocked { .. } => Some(LlmCapability::Blocked),
            Self::Abort { .. } => Some(LlmCapability::Abort),
            Self::UserLanguage { .. } => Some(LlmCapability::UserLanguage),
            Self::CustomTool { .. } => Some(LlmCapability::CustomTools),
            Self::ReviewUpdate { .. } => Some(LlmCapability::ReviewUpdate),
            Self::ProtocolError { .. } => None,
        }
    }

    /// Phase of the bounded turn this command belongs to, or `None` for a
    /// protocol diagnostic.
    pub fn phase(&self) -> Option<CapabilityPhase> {
        self.capability().map(LlmCapability::phase)
    }
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
    /// The task completed successfully, carrying the Git commit message of the change set.
    Success { commit_message: Option<String> },
    /// The task is blocked, with an explanatory message.
    Error { message: String },
    /// The task is skipped, with an explanatory reason.
    Skip { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let search = LlmCommand::RepoSearch {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        };
        let json = serde_json::to_string(&search).unwrap();
        assert!(json.contains("\"type\":\"repo_search\""));
        assert!(json.contains("\"path\":\"src\""));
        assert!(json.contains("\"regex\":\"fn main\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), search);

        let doc = LlmCommand::DocRequest {
            topic: DocTopic::Workflow,
        };
        let json = serde_json::to_string(&doc).unwrap();
        assert!(json.contains("\"type\":\"tauqe_doc_request\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), doc);
        // The pre-canonical spelling stays a declared alias, so a payload
        // written before the rename keeps decoding identically.
        let legacy: LlmCommand =
            serde_json::from_str(r#"{"type":"doc_request","topic":"workflow"}"#)
                .expect("the legacy spelling must decode");
        assert_eq!(legacy, doc);

        let outer_doc = LlmCommand::OuterDocRequest {
            id: "mylib".to_string(),
        };
        let json = serde_json::to_string(&outer_doc).unwrap();
        assert!(json.contains("\"type\":\"outer_doc_request\""));
        assert!(json.contains("\"id\":\"mylib\""));
        assert_eq!(
            serde_json::from_str::<LlmCommand>(&json).unwrap(),
            outer_doc
        );

        let dirs = LlmCommand::RepoDirs;
        let json = serde_json::to_string(&dirs).unwrap();
        assert!(json.contains("\"type\":\"repo_dirs\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), dirs);

        let map = LlmCommand::RepoMap {
            path: "crates/core".to_string(),
        };
        let json = serde_json::to_string(&map).unwrap();
        assert!(json.contains("\"type\":\"repo_map\""));
        assert!(json.contains("\"path\":\"crates/core\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), map);

        let dir_list = LlmCommand::DirList {
            path: "crates/core".to_string(),
            recursive: true,
        };
        let json = serde_json::to_string(&dir_list).unwrap();
        assert!(json.contains("\"type\":\"dir_list\""));
        assert!(json.contains("\"path\":\"crates/core\""));
        assert!(json.contains("\"recursive\":true"));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), dir_list);
    }
    /// Returns one payload per command that carries a repository path.
    ///
    /// Every entry addresses the same spelling, so one loop proves that every
    /// command applies the path rules the search command already applied.
    fn path_payloads(path: &str) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"type": "context_request", "path": path, "access": "read_only"}),
            serde_json::json!({"type": "repo_search", "path": path, "regex": "fn main"}),
            serde_json::json!({"type": "repo_map", "path": path}),
            serde_json::json!({"type": "dir_list", "path": path, "recursive": true}),
            serde_json::json!({"type": "edit", "operation": {"type": "delete", "path": path}}),
            serde_json::json!({
                "type": "edit",
                "operation": {"type": "move", "from": path, "to": "src/b.rs"},
            }),
            serde_json::json!({
                "type": "edit",
                "operation": {"type": "move_dir", "from": "src/old", "to": path},
            }),
        ]
    }

    #[test]
    fn test_every_command_path_is_normalized_or_rejected() {
        // A benign spelling reaches the engine already normalized, so no
        // consumer has to re-clean a path the boundary could have cleaned.
        let context: LlmCommand = serde_json::from_value(serde_json::json!({
            "type": "context_request",
            "path": " ./src/a.rs ",
            "access": "editable",
        }))
        .expect("a benign context path must decode");
        assert_eq!(
            context,
            LlmCommand::ContextRequest {
                path: "src/a.rs".to_string(),
                access: ContextAccess::Editable,
            }
        );

        let map: LlmCommand =
            serde_json::from_value(serde_json::json!({"type": "repo_map", "path": "./crates"}))
                .expect("a benign map path must decode");
        assert_eq!(
            map,
            LlmCommand::RepoMap {
                path: "crates".to_string(),
            }
        );

        // An empty path, `.`, and `./` name the whole repository on both
        // discovery variants that accept a subtree prefix.
        for spelling in ["", ".", "./"] {
            let listing: LlmCommand = serde_json::from_value(serde_json::json!({
                "type": "dir_list",
                "path": spelling,
                "recursive": false,
            }))
            .expect("a whole-repository listing path must decode");
            assert_eq!(
                listing,
                LlmCommand::DirList {
                    path: String::new(),
                    recursive: false,
                },
                "spelling '{spelling}' must name the whole repository"
            );

            let search: LlmCommand = serde_json::from_value(serde_json::json!({
                "type": "repo_search",
                "path": spelling,
                "regex": "fn main",
            }))
            .expect("a whole-repository search path must decode");
            assert_eq!(
                search,
                LlmCommand::RepoSearch {
                    path: String::new(),
                    regex: "fn main".to_string(),
                },
                "spelling '{spelling}' must name the whole repository"
            );
        }

        // A file edit carries its paths through the same boundary, so a move
        // arrives with both directories normalized.
        let edit: LlmCommand = serde_json::from_value(serde_json::json!({
            "type": "edit",
            "operation": {"type": "move", "from": "./src/a.rs", "to": " ./src/b.rs "},
        }))
        .expect("a benign move must decode");
        assert_eq!(
            edit,
            LlmCommand::Edit {
                operation: EditOperation::Move {
                    from: "src/a.rs".to_string(),
                    to: "src/b.rs".to_string(),
                },
            }
        );
    }

    #[test]
    fn test_every_command_path_rejects_an_escaping_spelling() {
        // One set of spellings covers every repository path the wire form
        // carries. A typed variant cannot silently drop itself, so the
        // rejection the search command applied must reach every one of them.
        for spelling in ["../escape", "/etc/passwd", "a/../b", r"C:\Windows"] {
            let expected = spelling.replace('\\', "/");
            for payload in path_payloads(spelling) {
                let error = serde_json::from_value::<LlmCommand>(payload.clone())
                    .expect_err("an escaping repository path must be rejected");
                assert!(
                    error.to_string().contains(&expected),
                    "diagnostic for {payload} must name the rejected path: {error}"
                );
            }
        }

        // A context request and a file edit address a file, so an empty path is
        // a rejection rather than the whole-repository spelling.
        for payload in [
            serde_json::json!({"type": "context_request", "path": "  ", "access": "read_only"}),
            serde_json::json!({"type": "edit", "operation": {"type": "delete", "path": " "}}),
        ] {
            let error = serde_json::from_value::<LlmCommand>(payload.clone())
                .expect_err("an empty file path must be rejected");
            assert!(
                error.to_string().contains("must not be empty"),
                "diagnostic for {payload} must name the empty path: {error}"
            );
        }
    }

    #[test]
    fn test_doc_request_without_topic_decodes_to_all() {
        // The documented missing-topic fallback holds at the command boundary,
        // under the canonical spelling and its declared alias alike.
        for spelling in ["tauqe_doc_request", "doc_request"] {
            let raw = format!(r#"{{"type":"{spelling}"}}"#);
            let command: LlmCommand =
                serde_json::from_str(&raw).expect("a doc request without a topic must decode");
            assert_eq!(
                command,
                LlmCommand::DocRequest {
                    topic: DocTopic::All
                }
            );
        }
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

        assert_eq!(
            serde_json::to_string(&VerifyTarget::All).unwrap(),
            "\"all\""
        );
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
        assert_eq!(
            serde_json::from_str::<LlmCommand>(&json).unwrap(),
            scratchpad
        );

        let language = LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        };
        let json = serde_json::to_string(&language).unwrap();
        assert!(json.contains("\"type\":\"user_language\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), language);

        let proto_err = LlmCommand::ProtocolError {
            message: "Malformed XML edit block: Unclosed tag".to_string(),
        };
        let json = serde_json::to_string(&proto_err).unwrap();
        assert!(json.contains("\"type\":\"protocol_error\""));
        assert!(json.contains("\"message\":\"Malformed XML edit block: Unclosed tag\""));
        assert_eq!(
            serde_json::from_str::<LlmCommand>(&json).unwrap(),
            proto_err
        );
    }

    #[test]
    fn test_llm_command_outcome_serde_roundtrip() {
        let success = LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                commit_message: Some("All checks passed".to_string()),
            },
        };
        let json = serde_json::to_string(&success).unwrap();
        assert!(json.contains("\"type\":\"outcome\""));
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"commit_message\":\"All checks passed\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), success);

        let error = LlmCommand::Outcome {
            outcome: LlmOutcome::Error {
                message: "Missing dependency".to_string(),
            },
        };
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("\"status\":\"error\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), error);

        let skip = LlmCommand::Outcome {
            outcome: LlmOutcome::Skip {
                reason: "Already implemented".to_string(),
            },
        };
        let json = serde_json::to_string(&skip).unwrap();
        assert!(json.contains("\"status\":\"skip\""));
        assert!(json.contains("\"reason\":\"Already implemented\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), skip);
    }

    #[test]
    fn test_llm_outcome_skip_serde_roundtrip() {
        let skip = LlmOutcome::Skip {
            reason: "Requirements already met".to_string(),
        };
        let json = serde_json::to_string(&skip).unwrap();
        assert_eq!(
            json,
            "{\"status\":\"skip\",\"reason\":\"Requirements already met\"}"
        );
        let decoded: LlmOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, skip);
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

    #[test]
    fn test_llm_command_abort_serde_roundtrip() {
        let abort = LlmCommand::Abort {
            reason: "Approval required".to_string(),
            recommendation: "Run pnpm approve-builds".to_string(),
        };
        let json = serde_json::to_string(&abort).unwrap();
        assert!(json.contains("\"type\":\"abort\""));
        assert!(json.contains("\"reason\":\"Approval required\""));
        assert!(json.contains("\"recommendation\":\"Run pnpm approve-builds\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), abort);
    }

    #[test]
    fn test_llm_command_capability_and_phase() {
        let text = LlmCommand::Text {
            content: "hello".to_string(),
        };
        assert_eq!(text.capability(), Some(LlmCapability::TextMessage));
        assert_eq!(text.phase(), Some(CapabilityPhase::Execution));

        let edit = LlmCommand::Edit {
            operation: EditOperation::Replace {
                path: "src/lib.rs".to_string(),
                old_text: "a".to_string(),
                new_text: "b".to_string(),
            },
        };
        assert_eq!(edit.capability(), Some(LlmCapability::FileEdits));
        assert_eq!(edit.phase(), Some(CapabilityPhase::Execution));

        let context = LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        };
        assert_eq!(context.capability(), Some(LlmCapability::Research));
        assert_eq!(context.phase(), Some(CapabilityPhase::Research));

        let search = LlmCommand::RepoSearch {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        };
        assert_eq!(search.capability(), Some(LlmCapability::Research));

        let dirs = LlmCommand::RepoDirs;
        assert_eq!(dirs.capability(), Some(LlmCapability::Research));
        assert_eq!(dirs.phase(), Some(CapabilityPhase::Research));

        let map = LlmCommand::RepoMap {
            path: "crates/core".to_string(),
        };
        assert_eq!(map.capability(), Some(LlmCapability::Research));
        assert_eq!(map.phase(), Some(CapabilityPhase::Research));

        let dir_list = LlmCommand::DirList {
            path: "crates/core".to_string(),
            recursive: false,
        };
        assert_eq!(dir_list.capability(), Some(LlmCapability::Research));
        assert_eq!(dir_list.phase(), Some(CapabilityPhase::Research));

        let doc = LlmCommand::DocRequest {
            topic: DocTopic::Workflow,
        };
        assert_eq!(doc.capability(), Some(LlmCapability::DocRequest));
        assert_eq!(doc.phase(), Some(CapabilityPhase::Research));

        let outer_doc = LlmCommand::OuterDocRequest {
            id: "mylib".to_string(),
        };
        assert_eq!(outer_doc.capability(), Some(LlmCapability::OuterDocRequest));
        assert_eq!(outer_doc.phase(), Some(CapabilityPhase::Research));

        let verify = LlmCommand::Verify {
            target: VerifyTarget::All,
            on_success: VerifyOnSuccess::Silent,
        };
        assert_eq!(verify.capability(), Some(LlmCapability::ToolchainVerify));
        assert_eq!(verify.phase(), Some(CapabilityPhase::Verification));

        let scratchpad = LlmCommand::Scratchpad {
            notes: "notes".to_string(),
        };
        assert_eq!(scratchpad.capability(), Some(LlmCapability::Scratchpad));

        let outcome = LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                commit_message: None,
            },
        };
        assert_eq!(outcome.capability(), Some(LlmCapability::Outcome));
        assert_eq!(outcome.phase(), Some(CapabilityPhase::Outcome));

        let blocked = LlmCommand::Blocked {
            reason: "blocked".to_string(),
            details: None,
        };
        assert_eq!(blocked.capability(), Some(LlmCapability::Blocked));

        let abort = LlmCommand::Abort {
            reason: "blocked by host".to_string(),
            recommendation: "run script".to_string(),
        };
        assert_eq!(abort.capability(), Some(LlmCapability::Abort));
        assert_eq!(abort.phase(), Some(CapabilityPhase::Outcome));

        let language = LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        };
        assert_eq!(language.capability(), Some(LlmCapability::UserLanguage));

        let custom_tool = LlmCommand::CustomTool {
            name: "migrate".to_string(),
            args: serde_json::json!({ "database": "primary" }),
        };
        assert_eq!(custom_tool.capability(), Some(LlmCapability::CustomTools));
        assert_eq!(custom_tool.phase(), Some(CapabilityPhase::Execution));

        let error = LlmCommand::ProtocolError {
            message: "malformed".to_string(),
        };
        assert_eq!(error.capability(), None);
        assert_eq!(error.phase(), None);

        let plan_save = LlmCommand::Plan {
            update: DiscussionPlanUpdate {
                action: DiscussionPlanAction::Save,
                ..Default::default()
            },
        };
        let save_capability = plan_save.capability();
        assert_eq!(save_capability, Some(LlmCapability::PlanCreation));

        for action in [DiscussionPlanAction::Update, DiscussionPlanAction::Delete] {
            let plan = LlmCommand::Plan {
                update: DiscussionPlanUpdate {
                    action,
                    ..Default::default()
                },
            };
            assert_eq!(plan.capability(), Some(LlmCapability::PlanUpdate));
        }
    }

    #[test]
    fn test_llm_command_custom_tool_serde_roundtrip() {
        let tool = LlmCommand::CustomTool {
            name: "migrate".to_string(),
            args: serde_json::json!({ "database": "primary", "steps": 2 }),
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"type\":\"custom_tool\""));
        assert!(json.contains("\"name\":\"migrate\""));
        assert!(json.contains("\"database\":\"primary\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), tool);

        // An invocation without arguments still decodes, so a command that
        // declares none never requires the model to invent an empty object.
        let bare: LlmCommand =
            serde_json::from_str(r#"{"type":"custom_tool","name":"status"}"#).unwrap();
        assert_eq!(
            bare,
            LlmCommand::CustomTool {
                name: "status".to_string(),
                args: serde_json::Value::Null,
            }
        );
    }
    #[test]
    fn test_llm_command_review_update_serde_roundtrip() {
        let command = LlmCommand::ReviewUpdate {
            update: crate::review::ReviewUpdateProposal {
                item_id: 7,
                group_id: None,
                status: crate::review::ReviewDecisionStatus::Rejected,
                note: "The reported race cannot happen".to_string(),
                recommendations: vec!["Document the ordering".to_string()],
            },
        };
        let json = serde_json::to_string(&command).unwrap();
        assert!(json.contains("\"type\":\"review_update\""));
        assert!(json.contains("\"status\":\"rejected\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), command);

        // The dialogue anchor authorizes a decision, not a capability module,
        // so the command carries neither a capability nor a phase of its own.
        // A decision is a capability of the review discussion regime, so the
        // command carries that capability and its execution phase.
        assert_eq!(command.capability(), Some(LlmCapability::ReviewUpdate));
        assert_eq!(command.phase(), Some(CapabilityPhase::Execution));
    }
}
