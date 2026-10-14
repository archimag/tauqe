use serde::{Deserialize, Serialize};

use crate::git::RepositoryState;
use crate::model::ModelRef;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
    /// Absent id, used for responses that cannot be correlated with a request
    /// (for example a request that failed to parse). Serialized as `null`.
    Null,
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
        match serde_json::to_value(result) {
            Ok(value) => Self {
                id,
                result: Some(value),
                error: None,
            },
            // A serialization failure must never masquerade as a successful
            // void result: surface it as a deterministic internal error so the
            // client can distinguish it from an empty success.
            Err(err) => Self::err(id, crate::errors::INTERNAL_ERROR, err.to_string()),
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

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Event(Event),
}

/// Permissive capture of every field a wire message may carry.
///
/// A message is classified by field presence rather than by untagged
/// trial-and-error. Deserializing into this intermediate shape first lets a
/// malformed request (for example one whose `method` is not a string) surface
/// as a parse error instead of being silently absorbed by the broader
/// [`Response`] shape.
#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<ResponseError>,
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawMessage::deserialize(deserializer)?;

        // A message carrying `method` is a client request (when it has an id)
        // or a server event (when it does not). Any other object is a response.
        match raw.method {
            Some(method) => match raw.id {
                Some(id) => Ok(Message::Request(Request {
                    id,
                    method,
                    params: raw.params,
                })),
                None => Ok(Message::Event(Event {
                    method,
                    params: raw.params,
                })),
            },
            None => Ok(Message::Response(Response {
                id: raw.id.unwrap_or(RequestId::Null),
                result: raw.result,
                error: raw.error,
            })),
        }
    }
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
    fn test_request_id_null_roundtrip() {
        let json = serde_json::to_string(&RequestId::Null).unwrap();
        assert_eq!(json, "null");

        let de: RequestId = serde_json::from_str("null").unwrap();
        assert_eq!(de, RequestId::Null);
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

    #[test]
    fn test_message_rejects_malformed_request_instead_of_misclassifying() {
        // A request whose `method` is not a string must surface as a parse
        // error rather than being silently absorbed by the response shape.
        assert!(serde_json::from_str::<Message>(r#"{"id":1,"method":123}"#).is_err());

        // A request carrying a null id is not a valid request; it is routed to a
        // server event, so no reply is ever produced on a null correlation id.
        match serde_json::from_str::<Message>(r#"{"id":null,"method":"ping"}"#).unwrap() {
            Message::Event(event) => assert_eq!(event.method, "ping"),
            other => panic!("Expected Event, got {other:?}"),
        }

        // An uncorrelated error response still round-trips with a null id.
        let uncorrelated = Response::err(RequestId::Null, "PARSE_ERROR", "bad request");
        let json = serde_json::to_string(&uncorrelated).unwrap();
        match serde_json::from_str::<Message>(&json).unwrap() {
            Message::Response(response) => assert_eq!(response.id, RequestId::Null),
            other => panic!("Expected Response, got {other:?}"),
        }
    }
}
