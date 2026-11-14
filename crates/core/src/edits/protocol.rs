use tauqe_protocol::{ContextAccess, EditProtocolKind};

pub mod structured;
pub mod utils;
pub use structured::StructuredEditProtocol;

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

impl TerminalOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub fn commit_message(&self) -> Option<&str> {
        match self {
            Self::Success { commit_message } => commit_message.as_deref(),
            _ => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error { message } => Some(message.as_str()),
            _ => None,
        }
    }

    pub fn blocked_reason(&self) -> Option<&str> {
        match self {
            Self::Blocked { reason, .. } => Some(reason.as_str()),
            _ => None,
        }
    }

    pub fn blocked_details(&self) -> Option<&str> {
        match self {
            Self::Blocked { details, .. } => details.as_deref(),
            _ => None,
        }
    }
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
    pub target: VerifyTarget,
    pub on_success: VerifyOnSuccess,
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

    /// Validates whether an edit protocol name is recognized.
    pub fn is_valid(name: &str) -> bool {
        Self::canonical_name(name).is_some()
    }

    /// Resolves aliases to a canonical edit protocol kind.
    pub fn canonical_name(name: &str) -> Option<EditProtocolKind> {
        name.parse::<EditProtocolKind>().ok()
    }

    /// Creates a [`PromptProtocol`](crate::prompt::protocol::PromptProtocol)
    /// instance by protocol kind.
    ///
    /// The factory returns the unified prompt protocol contract, so callers
    /// never depend on a separate edit-protocol hierarchy.
    ///
    /// `Mixed` resolves to the XML contour, because code edits never travel
    /// through the structured contour in that mode; the structured transport of
    /// the mode is reserved for discussions, commit messages, and history
    /// compaction, which are resolved by the workflows that own them.
    pub fn create_protocol(
        _kind: EditProtocolKind,
    ) -> Box<dyn crate::prompt::protocol::PromptProtocol> {
        Box::new(crate::prompt::json::JsonPromptProtocol::edits())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_protocol_factory_returns_prompt_protocol() {
        let structured = EditProtocolFactory::create_protocol(EditProtocolKind::Structured);
        assert_eq!(structured.name(), "structured");
    }

    #[test]
    fn test_canonical_name_resolves_aliases() {
        assert_eq!(
            EditProtocolFactory::canonical_name("structured"),
            Some(EditProtocolKind::Structured)
        );
        assert_eq!(
            EditProtocolFactory::canonical_name("xml"),
            Some(EditProtocolKind::Structured)
        );
        assert_eq!(
            EditProtocolFactory::canonical_name("mixed"),
            Some(EditProtocolKind::Structured)
        );
        assert_eq!(
            EditProtocolFactory::canonical_name("auto"),
            Some(EditProtocolKind::Structured)
        );
        assert_eq!(EditProtocolFactory::canonical_name("tool_call"), None);
        assert_eq!(
            EditProtocolFactory::available_protocols(),
            vec![EditProtocolKind::Structured]
        );
    }
}
