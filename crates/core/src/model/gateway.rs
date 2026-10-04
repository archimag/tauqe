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
    Usage(ModelUsageInfo),
    Done,
    Cancelled,
    Error(String),
}
