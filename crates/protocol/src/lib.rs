use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: &str = "0.1.0";

pub mod methods {
    pub const CLIENT_INITIALIZE: &str = "client/initialize";
    pub const REPOSITORY_GET_STATE: &str = "repository/getState";
    pub const REPOSITORY_LIST_FILES: &str = "repository/listFiles";
    pub const MODEL_ASK: &str = "model/ask";
    pub const MODEL_CANCEL: &str = "model/cancel";
    pub const MODEL_CLEAR_HISTORY: &str = "model/clearHistory";

    pub const CONTEXT_GET: &str = "context/get";
    pub const CONTEXT_ADD: &str = "context/add";
    pub const CONTEXT_REMOVE: &str = "context/remove";
    pub const CONTEXT_SET_ACCESS: &str = "context/setAccess";
    pub const CONTEXT_CLEAR: &str = "context/clear";
}

pub mod events {
    pub const MODEL_STARTED: &str = "model/started";
    pub const MODEL_REASONING_DELTA: &str = "model/reasoningDelta";
    pub const MODEL_TEXT_DELTA: &str = "model/textDelta";
    pub const MODEL_USAGE: &str = "model/usage";
    pub const MODEL_FINISHED: &str = "model/finished";
    pub const MODEL_CANCELLED: &str = "model/cancelled";
    pub const MODEL_ERROR: &str = "model/error";

    pub const CONTEXT_CHANGED: &str = "context/changed";
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub client_name: String,
    pub client_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
    pub repository: Option<RepositoryState>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryState {
    pub root: String,
    pub branch: String,
    pub head: String,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryListFilesResult {
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAccess {
    ReadOnly,
    Editable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    pub path: String,
    pub access: ContextAccess,
    pub size_bytes: u64,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextState {
    pub revision: u64,
    pub items: Vec<ContextItem>,
    pub total_estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAddParams {
    pub path: String,
    #[serde(default = "default_context_access")]
    pub access: ContextAccess,
}

fn default_context_access() -> ContextAccess {
    ContextAccess::ReadOnly
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRemoveParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSetAccessParams {
    pub path: String,
    pub access: ContextAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextChangedEvent {
    pub state: ContextState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAskParams {
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStartedEvent {
    pub operation_id: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeltaEvent {
    pub operation_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelUsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageEvent {
    pub operation_id: String,
    pub usage: ModelUsageInfo,
    pub session_total_cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFinishedEvent {
    pub operation_id: String,
    pub full_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelErrorEvent {
    pub operation_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_id_serialization() {
        let id_num = RequestId::Number(42);
        assert_eq!(serde_json::to_string(&id_num).unwrap(), "42");

        let id_str = RequestId::String("req-1".to_string());
        assert_eq!(serde_json::to_string(&id_str).unwrap(), "\"req-1\"");
    }

    #[test]
    fn test_request_id_deserialization() {
        let id_num: RequestId = serde_json::from_str("42").unwrap();
        assert_eq!(id_num, RequestId::Number(42));

        let id_str: RequestId = serde_json::from_str("\"req-1\"").unwrap();
        assert_eq!(id_str, RequestId::String("req-1".to_string()));
    }

    #[test]
    fn test_message_deserialization_request() {
        let json_str = r#"{"id": 1, "method": "client/initialize", "params": {}}"#;
        let msg: Message = serde_json::from_str(json_str).unwrap();
        match msg {
            Message::Request(req) => {
                assert_eq!(req.id, RequestId::Number(1));
                assert_eq!(req.method, "client/initialize");
                assert!(req.params.is_some());
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_context_access_serialization() {
        let ro = ContextAccess::ReadOnly;
        assert_eq!(serde_json::to_string(&ro).unwrap(), "\"read_only\"");
        let ed = ContextAccess::Editable;
        assert_eq!(serde_json::to_string(&ed).unwrap(), "\"editable\"");
    }
}
