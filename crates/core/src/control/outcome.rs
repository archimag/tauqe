//! Outcome classifications, result envelopes, and failure reports.

use tauqe_protocol::{LlmCapability, LlmCommand, ModelUsageInfo};

/// Infrastructure-level failure that aborts execution without consuming model attempts.
#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    #[error("operation cancelled")]
    Cancelled,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("infrastructure error: {0}")]
    Other(String),
}

/// Drainable model response envelope containing semantic commands keyed by capability.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponseEnvelope {
    /// Semantic commands produced by the model.
    items: Vec<LlmCommand>,
    /// Raw assistant text prior to stripping.
    pub raw_text: String,
    /// Token usage metrics for the round.
    pub usage: Option<ModelUsageInfo>,
}

impl ResponseEnvelope {
    /// Constructs an envelope from a list of decoded semantic commands.
    pub fn new(items: Vec<LlmCommand>) -> Self {
        Self {
            items,
            raw_text: String::new(),
            usage: None,
        }
    }

    /// Constructs an envelope by cloning a slice of decoded semantic commands.
    pub fn from_commands(commands: &[LlmCommand]) -> Self {
        Self {
            items: commands.to_vec(),
            raw_text: String::new(),
            usage: None,
        }
    }

    /// Attaches token usage information.
    pub fn with_usage(mut self, usage: Option<ModelUsageInfo>) -> Self {
        self.usage = usage;
        self
    }

    /// Attaches raw unstripped model response text.
    pub fn with_raw_text(mut self, raw_text: impl Into<String>) -> Self {
        self.raw_text = raw_text.into();
        self
    }

    /// Drains and returns all commands matching the specified capability.
    pub fn take(&mut self, capability: LlmCapability) -> Vec<LlmCommand> {
        let mut matched = Vec::new();
        let mut remaining = Vec::with_capacity(self.items.len());
        for cmd in self.items.drain(..) {
            if cmd.capability() == Some(capability) {
                matched.push(cmd);
            } else {
                remaining.push(cmd);
            }
        }
        self.items = remaining;
        matched
    }

    /// Drains and returns all commands matching any of the specified capabilities.
    pub fn take_any(&mut self, capabilities: &[LlmCapability]) -> Vec<LlmCommand> {
        let mut matched = Vec::new();
        let mut remaining = Vec::with_capacity(self.items.len());
        for cmd in self.items.drain(..) {
            if cmd
                .capability()
                .is_some_and(|cap| capabilities.contains(&cap))
            {
                matched.push(cmd);
            } else {
                remaining.push(cmd);
            }
        }
        self.items = remaining;
        matched
    }

    /// Returns whether any command matching the capability is present.
    pub fn has(&self, capability: LlmCapability) -> bool {
        self.items
            .iter()
            .any(|cmd| cmd.capability() == Some(capability))
    }

    /// Returns the unique set of capabilities remaining in the envelope.
    pub fn remaining_capabilities(&self) -> Vec<LlmCapability> {
        let mut caps: Vec<_> = self.items.iter().filter_map(|c| c.capability()).collect();
        caps.sort_by_key(|c| *c as usize);
        caps.dedup();
        caps
    }

    /// Returns true if all commands in the envelope have been consumed.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns the number of commands remaining in the envelope.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns a slice of the commands currently remaining in the envelope.
    pub fn commands(&self) -> &[LlmCommand] {
        &self.items
    }
}

/// Successful result of a control loop pairing the specific artifact with the response envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopSuccess<T> {
    /// Domain artifact produced by the loop (e.g. StagedEdits or VerificationReport).
    pub value: T,
    /// Full response envelope from the model preserving companion fields.
    pub response: ResponseEnvelope,
}

impl<T> LoopSuccess<T> {
    /// Wraps an artifact with its originating model response envelope.
    pub fn new(value: T, response: ResponseEnvelope) -> Self {
        Self { value, response }
    }
}

/// Summary of localized iteration failures when the retry budget is exhausted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureReport {
    /// Primary failure summary.
    pub reason: String,
    /// Number of local iterations attempted.
    pub attempts: usize,
    /// Detailed diagnostics from compiler, linter, or patch engine.
    pub diagnostics: Vec<String>,
}

impl FailureReport {
    /// Creates a new failure report.
    pub fn new(reason: impl Into<String>, attempts: usize, diagnostics: Vec<String>) -> Self {
        Self {
            reason: reason.into(),
            attempts,
            diagnostics,
        }
    }
}

/// Developer-facing report of a semantic `<abort>` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortReport {
    /// What blocked the work in the host environment.
    pub reason: String,
    /// The exact command or instruction the developer must run.
    pub recommendation: String,
}

impl AbortReport {
    /// Creates a new abort report.
    pub fn new(reason: impl Into<String>, recommendation: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            recommendation: recommendation.into(),
        }
    }

    /// Renders the report as a developer-facing message.
    pub fn to_message(&self) -> String {
        format!(
            "Execution aborted: {}\n\nRecommended action:\n{}",
            self.reason.trim(),
            self.recommendation.trim()
        )
    }
}

/// Domain-level outcome of a control loop.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopResult<T> {
    /// 1. Target artifact successfully generated and validated.
    Success(LoopSuccess<T>),
    /// 2. Model responded within active capabilities with a non-target message (e.g. Blocked or Text).
    Diverged(ResponseEnvelope),
    /// 3. Model attempted the task but local healing iterations exceeded their budget.
    Exhausted(FailureReport),
}

impl<T> LoopResult<T> {
    /// Unwraps the success artifact if this outcome is [`LoopResult::Success`].
    pub fn into_success(self) -> Option<LoopSuccess<T>> {
        match self {
            Self::Success(s) => Some(s),
            _ => None,
        }
    }

    /// Returns true if the loop completed successfully.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }

    /// Returns true if the loop diverged.
    pub fn is_diverged(&self) -> bool {
        matches!(self, Self::Diverged(_))
    }

    /// Returns true if the loop exhausted its attempts.
    pub fn is_exhausted(&self) -> bool {
        matches!(self, Self::Exhausted(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{BlockedCommand, CommitMessageCommand, TextCommand};
    #[test]
    fn test_drainable_envelope() {
        let commands = vec![
            LlmCommand::Text(TextCommand {
                content: "Fixed the bug.".to_string(),
            }),
            LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: "fix(core): resolve null pointer".to_string(),
            }),
            LlmCommand::Blocked(BlockedCommand {
                reason: "Database unavailable".to_string(),
                details: "connection refused on port 5432".to_string(),
            }),
        ];

        let mut resp = ResponseEnvelope::from_commands(&commands);
        assert!(!resp.is_empty());
        assert_eq!(resp.len(), 3);

        let caps = resp.remaining_capabilities();
        assert!(caps.contains(&LlmCapability::TextMessage));
        assert!(caps.contains(&LlmCapability::CommitMessage));

        // Drain TextMessage
        let text_cmds = resp.take(LlmCapability::TextMessage);
        assert_eq!(text_cmds.len(), 1);
        match &text_cmds[0] {
            LlmCommand::Text(command) => assert_eq!(command.content, "Fixed the bug."),
            _ => panic!("expected text command"),
        }
        assert!(!resp.has(LlmCapability::TextMessage));
        assert_eq!(resp.len(), 2);
        // Drain Outcome and Blocked together
        let drained = resp.take_any(&[LlmCapability::CommitMessage, LlmCapability::Blocked]);
        assert_eq!(drained.len(), 2);
        assert!(resp.is_empty());
        assert!(resp.remaining_capabilities().is_empty());
    }

    #[test]
    fn test_loop_success_envelope() {
        let commands = vec![LlmCommand::Text(TextCommand {
            content: "Explanation text".to_string(),
        })];
        let resp = ResponseEnvelope::from_commands(&commands);
        let success = LoopSuccess::new(42, resp);
        assert_eq!(success.value, 42);
        assert_eq!(success.response.len(), 1);

        let outcome: LoopResult<i32> = LoopResult::Success(success);
        assert!(outcome.is_success());
        assert!(!outcome.is_diverged());
        assert!(!outcome.is_exhausted());

        let unwrapped = outcome.into_success().expect("success");
        assert_eq!(unwrapped.value, 42);
    }
}
