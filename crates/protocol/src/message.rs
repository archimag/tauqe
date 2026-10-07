use serde::{Deserialize, Serialize};

use crate::git::RepositoryState;
use crate::model::ModelRef;

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

impl Response {
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn ok_typed<T: Serialize>(id: RequestId, result: &T) -> Self {
        Self {
            id,
            result: serde_json::to_value(result).ok(),
            error: None,
        }
    }

    pub fn err(id: RequestId, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(ResponseError::new(code, message)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ResponseError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(
        code: impl Into<String>,
        message: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: Some(data),
        }
    }
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
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_workflows: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_edit_protocols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<ModelRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_id_serialization() {
        let num_id = RequestId::Number(42);
        let str_id = RequestId::String("req-123".to_string());

        let num_json = serde_json::to_string(&num_id).unwrap();
        let str_json = serde_json::to_string(&str_id).unwrap();

        assert_eq!(num_json, "42");
        assert_eq!(str_json, "\"req-123\"");

        let de_num: RequestId = serde_json::from_str("42").unwrap();
        let de_str: RequestId = serde_json::from_str("\"req-123\"").unwrap();

        assert_eq!(de_num, num_id);
        assert_eq!(de_str, str_id);
    }

    #[test]
    fn test_message_untagged_deserialization() {
        let req_json = r#"{"id":1,"method":"ping","params":{"foo":"bar"}}"#;
        let resp_json = r#"{"id":1,"result":{"ok":true}}"#;
        let err_json = r#"{"id":1,"error":{"code":"ERR","message":"failed"}}"#;
        let event_json = r#"{"method":"model/delta","params":{"delta":"abc"}}"#;

        match serde_json::from_str::<Message>(req_json).unwrap() {
            Message::Request(r) => {
                assert_eq!(r.id, RequestId::Number(1));
                assert_eq!(r.method, "ping");
            }
            _ => panic!("Expected Request"),
        }

        match serde_json::from_str::<Message>(resp_json).unwrap() {
            Message::Response(r) => {
                assert_eq!(r.id, RequestId::Number(1));
                assert!(r.result.is_some());
                assert!(r.error.is_none());
            }
            _ => panic!("Expected Response"),
        }

        match serde_json::from_str::<Message>(err_json).unwrap() {
            Message::Response(r) => {
                assert_eq!(r.id, RequestId::Number(1));
                let err = r.error.unwrap();
                assert_eq!(err.code, "ERR");
                assert_eq!(err.message, "failed");
            }
            _ => panic!("Expected Response"),
        }

        match serde_json::from_str::<Message>(event_json).unwrap() {
            Message::Event(e) => {
                assert_eq!(e.method, "model/delta");
            }
            _ => panic!("Expected Event"),
        }
    }
}
