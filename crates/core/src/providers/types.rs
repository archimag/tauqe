use serde::{Deserialize, Serialize};
use tauqe_protocol::ModelUsageInfo;

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
    pub schema: serde_json::Value,
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
        op_type: String,
    },
    EditHunk {
        path: String,
        hunk_index: usize,
        old_text: String,
        new_text: String,
    },
    EditFileDone {
        path: String,
        status: String,
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
    ToolchainStarted {
        command: String,
    },
    ToolchainFinished {
        command: String,
        success: bool,
        message: Option<String>,
    },

    Usage(ModelUsageInfo),
    Done,
    Cancelled,
    Error(String),
}
