use crate::context::ContextAccess;
use crate::workflow::EditProtocolKind;

pub mod structured;
pub(crate) mod utils;

/// Universal terminal outcome emitted by the model at the conclusion of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutcome {
    Success {
        commit_message: Option<String>,
    },
    Error {
        message: String,
    },
    Blocked {
        reason: String,
        details: Option<String>,
    },
    Skip {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyTarget {
    All,
    Check,
    Test,
    Clippy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOnSuccess {
    Silent,
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRequest {
    target: VerifyTarget,
    on_success: VerifyOnSuccess,
}

/// A file the model asks to be added to the context before it proposes edits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ContextRequest {
    pub path: String,
    pub access: ContextAccess,
}

/// Factory responsible for discovering, validating, and creating EditProtocol instances.
pub struct EditProtocolFactory;

impl EditProtocolFactory {
    /// Returns the list of standard supported edit protocol kinds.
    pub fn available_protocols() -> Vec<EditProtocolKind> {
        vec![EditProtocolKind::Structured]
    }
}
