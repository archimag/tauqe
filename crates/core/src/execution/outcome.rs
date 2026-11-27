//! Request facts, independent of budgets and scenario completion policy.

use crate::llm::{LlmCapability, LlmCommand};

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error(transparent)]
    InputLimit(#[from] super::budget::InputLimitExceeded),
    #[error("operation cancelled")]
    Cancelled,
    #[error("usage execution is not accepting provider requests")]
    UsageNotActive,
    #[error("provider error: {0}")]
    Provider(String),
    #[error("the provider stream closed without completing")]
    Incomplete,
    #[error("the provider returned an empty response")]
    EmptyResponse,
    #[error("invalid model response: {0}")]
    Protocol(String),
    #[error("infrastructure error: {0}")]
    Other(String),
}

/// Decoded semantic commands, without the raw provider payload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponseEnvelope {
    items: Vec<LlmCommand>,
}

impl ResponseEnvelope {
    pub fn from_commands(commands: &[LlmCommand]) -> Self {
        Self {
            items: commands.to_vec(),
        }
    }

    pub fn take(&mut self, capability: LlmCapability) -> Vec<LlmCommand> {
        self.take_any(&[capability])
    }

    pub fn take_any(&mut self, capabilities: &[LlmCapability]) -> Vec<LlmCommand> {
        let mut matched = Vec::new();
        let mut remaining = Vec::with_capacity(self.items.len());
        for cmd in self.items.drain(..) {
            if crate::llm::command_capability(&cmd).is_some_and(|cap| capabilities.contains(&cap)) {
                matched.push(cmd);
            } else {
                remaining.push(cmd);
            }
        }
        self.items = remaining;
        matched
    }

    pub fn has(&self, capability: LlmCapability) -> bool {
        self.items
            .iter()
            .any(|cmd| crate::llm::command_capability(cmd) == Some(capability))
    }

    pub fn remaining_capabilities(&self) -> Vec<LlmCapability> {
        let mut caps: Vec<_> = self
            .items
            .iter()
            .filter_map(crate::llm::command_capability)
            .collect();
        caps.sort_by_key(|c| *c as usize);
        caps.dedup();
        caps
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub fn commands(&self) -> &[LlmCommand] {
        &self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{BlockedCommand, CommitMessageCommand, TextCommand};

    #[test]
    fn draining_commands_preserves_unselected_capabilities_and_order() {
        let commands = [
            LlmCommand::Text(TextCommand {
                content: "Fixed the bug.".into(),
            }),
            LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: "fix(core): resolve null pointer".into(),
            }),
            LlmCommand::Blocked(BlockedCommand {
                reason: "Missing prerequisite".into(),
                details: String::new(),
            }),
        ];
        let mut envelope = ResponseEnvelope::from_commands(&commands);
        assert_eq!(envelope.len(), 3);
        assert!(envelope
            .remaining_capabilities()
            .contains(&LlmCapability::TextMessage));
        assert_eq!(envelope.take(LlmCapability::TextMessage), commands[..1]);
        assert!(!envelope.has(LlmCapability::TextMessage));
        assert_eq!(envelope.len(), 2);
        assert_eq!(
            envelope.take_any(&[LlmCapability::Blocked, LlmCapability::CommitMessage]),
            commands[1..]
        );
        assert!(envelope.is_empty());
        assert!(envelope.remaining_capabilities().is_empty());
    }
}
