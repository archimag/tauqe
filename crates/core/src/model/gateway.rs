use serde::{Deserialize, Serialize};
use workbench_protocol::ModelUsageInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ReasoningDelta(String),
    TextDelta(String),

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

    Usage(ModelUsageInfo),
    Done,
    Cancelled,
    Error(String),
}
