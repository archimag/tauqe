use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use tauqe_protocol::{EditFileOp, EditFileStatus, ModelRef, ModelRole, ModelUsageInfo};

pub use tauqe_protocol::ReasoningEffort;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    #[serde(
        default,
        serialize_with = "serialize_content",
        deserialize_with = "deserialize_content"
    )]
    pub content: String,
}

fn serialize_content<S>(content: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if content.is_empty() {
        serializer.serialize_none()
    } else {
        serializer.serialize_str(content)
    }
}

fn deserialize_content<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema { json_schema: JsonSchemaDefinition },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonSchemaDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: Cow<'static, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ReasoningDelta(String),
    TextDelta(String),

    // Context streaming events
    ContextChanged(tauqe_protocol::ContextState),

    // Semantic edit streaming events
    EditStarted,
    EditFileStarted {
        path: String,
        op_type: EditFileOp,
    },
    EditHunk {
        path: String,
        hunk_index: usize,
        /// One-based first line of the hunk in the file before the change.
        ///
        /// `0` states that the position is unknown.
        old_start: usize,
        /// Number of source lines the hunk covers.
        old_lines: usize,
        /// One-based first line of the hunk in the file after the change.
        new_start: usize,
        /// Number of result lines the hunk covers.
        new_lines: usize,
        /// Section the hunk belongs to, as a diff header names it after its ranges.
        context_hint: Option<String>,
        old_text: String,
        new_text: String,
    },
    EditFileDone {
        path: String,
        status: EditFileStatus,
        error: Option<String>,
        hunks_count: usize,
    },
    EditFileRetrying {
        path: String,
        attempt: usize,
        max_retries: usize,
        reason: String,
    },

    // Turn phase and toolchain telemetry
    TurnPhase {
        phase: tauqe_protocol::TurnPhase,
        round: Option<usize>,
        max_rounds: Option<usize>,
        detail: Option<String>,
    },
    RepoMapTokens(usize),
    /// The turn advanced to the next rung of the model ladder.
    ///
    /// A promoted rung changes the effective model, its reasoning effort, and
    /// the session affinity every later request addresses, so the runner reports
    /// the promotion as a semantic event instead of leaving the client to infer
    /// it from a model name it never sees.
    TierEscalated {
        /// Ladder position the turn moved to, as declared by the configuration.
        tier: usize,
        /// Model that now serves the turn.
        model: ModelRef,
        /// Semantic role the ladder belongs to.
        role: ModelRole,
        /// Reasoning effort of the promoted rung.
        effort: Option<ReasoningEffort>,
    },
    ToolchainStarted {
        command: String,
    },
    ToolchainFinished {
        command: String,
        success: bool,
        message: Option<String>,
    },

    /// A transient connection failure is being retried after a backoff pause.
    RetryingConnection {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: String,
    },
    /// The stream broke after partial output, so accumulated deltas are void.
    ResetStream,

    Usage(ModelUsageInfo),
    Done,
    Cancelled,
    Error(String),
}
