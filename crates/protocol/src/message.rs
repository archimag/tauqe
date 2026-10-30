use serde::{Deserialize, Serialize};

use crate::config::{EditProtocolKind, WorkflowKind};
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

#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

fn is_sensitive_method(method: &str) -> bool {
    method == crate::methods::CREDENTIALS_SAVE || method.starts_with("credentials/")
}

/// Usage-metric keys that must stay readable in observability output.
///
/// The list is an exact allowlist on purpose. Matching by suffix would let a
/// plural credential key such as `access_tokens` or `refresh_tokens` pass as a
/// usage metric and print in plaintext, so only these known metric names are
/// exempt from redaction.
const TOKEN_METRIC_KEYS: &[&str] = &[
    "tokens",
    "prompttokens",
    "completiontokens",
    "totaltokens",
    "reasoningtokens",
    "cachedtokens",
    "estimatedtokens",
    "addedtokens",
    "tokencount",
    "tokencounts",
];

/// Reports whether a JSON object key names a metric whose numeric value is
/// allowed to stay readable in observability output.
///
/// The exemption is keyed on the name only. Whether the bound value may pass
/// through unredacted is decided separately, because the allowlist exists for
/// usage counters and must never become a hole through which a string secret
/// travels.
fn is_token_metric_key(normalized: &str) -> bool {
    TOKEN_METRIC_KEYS.contains(&normalized)
}
/// Reports whether a JSON object key names a credential field.
///
/// Matching is tolerant of case and separator differences — `api_key`,
/// `api-key`, `apiKey`, and `openrouter_api_key` all collapse to the same
/// normalized form — and recognizes credential key *families* rather than a
/// fixed whitelist of exact names, so variants such as `access_token`,
/// `refresh_token`, `client_secret`, `authorization`, and `bearer` cannot slip
/// through in plaintext.
///
/// Usage counters such as `prompt_tokens` or `estimated_tokens` belong to the
/// same `token` family, so a key name alone never exempts them. The exemption
/// lives in `redact_sensitive_json`, which applies it only after the bound
/// value turns out to be a numeric counter.
fn is_sensitive_key(key: &str) -> bool {
    let normalized = normalize_key(key);

    normalized.ends_with("key")
        || normalized.ends_with("keys")
        || normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("password")
        || normalized.contains("credential")
        || normalized.contains("authorization")
        || normalized.contains("bearer")
}

/// Normalizes a JSON object key into its comparison form.
///
/// Non-alphanumeric separators are stripped and ASCII letters are lowercased,
/// so `api_key`, `api-key`, and `apiKey` collapse to the same string.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn redact_sensitive_json(value: &serde_json::Value, sensitive_context: bool) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sanitized = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                // A metric key exempts only a numeric counter. A string, array,
                // or object bound to the same key is not a usage measurement and
                // falls through to the ordinary sensitivity tests, so a secret
                // cannot ride in under an allowlisted name.
                let metric_exempt = is_token_metric_key(&normalize_key(k))
                    && matches!(v, serde_json::Value::Number(_));
                if sensitive_context || (is_sensitive_key(k) && !metric_exempt) {
                    if v.is_object() || v.is_array() {
                        sanitized.insert(k.clone(), redact_sensitive_json(v, true));
                    } else {
                        sanitized.insert(
                            k.clone(),
                            serde_json::Value::String("<redacted>".to_string()),
                        );
                    }
                } else if v.is_object() || v.is_array() {
                    sanitized.insert(k.clone(), redact_sensitive_json(v, false));
                } else {
                    sanitized.insert(k.clone(), v.clone());
                }
            }
            serde_json::Value::Object(sanitized)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| redact_sensitive_json(v, sensitive_context))
                .collect(),
        ),
        _ if sensitive_context => serde_json::Value::String("<redacted>".to_string()),
        other => other.clone(),
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut builder = f.debug_struct("Request");
        builder.field("id", &self.id);
        builder.field("method", &self.method);
        let sensitive = is_sensitive_method(&self.method);
        match &self.params {
            Some(params) => {
                let redacted = redact_sensitive_json(params, sensitive);
                builder.field("params", &Some(redacted));
            }
            None => {
                builder.field("params", &Option::<serde_json::Value>::None);
            }
        }
        builder.finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut builder = f.debug_struct("Response");
        builder.field("id", &self.id);
        match &self.result {
            Some(result) => {
                let redacted = redact_sensitive_json(result, false);
                builder.field("result", &Some(redacted));
            }
            None => {
                builder.field("result", &Option::<serde_json::Value>::None);
            }
        }
        builder.field("error", &self.error);
        builder.finish()
    }
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

#[derive(Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Debug for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut builder = f.debug_struct("ResponseError");
        builder.field("code", &self.code);
        builder.field("message", &self.message);
        match &self.data {
            Some(data) => {
                let redacted = redact_sensitive_json(data, false);
                builder.field("data", &Some(redacted));
            }
            None => {
                builder.field("data", &Option::<serde_json::Value>::None);
            }
        }
        builder.finish()
    }
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

#[derive(Clone, Serialize, Deserialize)]
pub struct Event {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut builder = f.debug_struct("Event");
        builder.field("method", &self.method);
        let sensitive = is_sensitive_method(&self.method);
        match &self.params {
            Some(params) => {
                let redacted = redact_sensitive_json(params, sensitive);
                builder.field("params", &Some(redacted));
            }
            None => {
                builder.field("params", &Option::<serde_json::Value>::None);
            }
        }
        builder.finish()
    }
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Event(Event),
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(req) => f.debug_tuple("Request").field(req).finish(),
            Self::Response(resp) => f.debug_tuple("Response").field(resp).finish(),
            Self::Event(event) => f.debug_tuple("Event").field(event).finish(),
        }
    }
}

/// Deserializes an `id` field that is present in the wire payload.
///
/// Serde invokes this only when the `id` key exists, so a JSON `null` maps to
/// `RequestId::Null` and yields `Some(RequestId::Null)`. An absent key instead
/// falls back to `None` through `#[serde(default)]`, letting the classifier
/// tell a request that carries a null id apart from an event that carries no
/// id at all.
fn deserialize_present_id<'de, D>(deserializer: D) -> Result<Option<RequestId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    RequestId::deserialize(deserializer).map(Some)
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
    #[serde(default, deserialize_with = "deserialize_present_id")]
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

        // A message carrying `method` is a client request when the `id` key is
        // present — including an explicit `null`, which classifies as
        // `RequestId::Null` so the server can reject it deterministically — and
        // a server event when the `id` key is entirely absent. Any other object
        // is a response.
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
            None => {
                // Without `method` the payload can only be a response, and a
                // response is meaningful only when it carries at least one of
                // `id`, `result`, or `error`. An empty or unrelated object
                // (`{}`, `{"foo":"bar"}`) is a malformed wire payload and must
                // surface as a parse error instead of being silently accepted
                // as a vacuous response.
                if raw.id.is_none() && raw.result.is_none() && raw.error.is_none() {
                    return Err(serde::de::Error::custom(
                        "message object carries neither `method`, `id`, `result`, nor `error`",
                    ));
                }

                Ok(Message::Response(Response {
                    id: raw.id.unwrap_or(RequestId::Null),
                    result: raw.result,
                    error: raw.error,
                }))
            }
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
    pub workflow: Option<WorkflowKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_protocol: Option<EditProtocolKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_workflows: Vec<WorkflowKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_edit_protocols: Vec<EditProtocolKind>,
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

        // A request carrying an explicitly null id is still a request: the
        // server then rejects it deterministically instead of silently dropping
        // it, so a malformed client is never left waiting.
        match serde_json::from_str::<Message>(r#"{"id":null,"method":"ping"}"#).unwrap() {
            Message::Request(request) => {
                assert_eq!(request.id, RequestId::Null);
                assert_eq!(request.method, "ping");
            }
            other => panic!("Expected Request with null id, got {other:?}"),
        }

        // A message that omits the `id` key entirely is a server event.
        match serde_json::from_str::<Message>(r#"{"method":"model/delta"}"#).unwrap() {
            Message::Event(event) => assert_eq!(event.method, "model/delta"),
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

    #[test]
    fn test_message_rejects_vacuous_object_instead_of_accepting_as_response() {
        // An object without `method`, `id`, `result`, or `error` carries no
        // message and must surface as a parse error instead of being silently
        // absorbed as a vacuous response.
        assert!(serde_json::from_str::<Message>("{}").is_err());
        assert!(serde_json::from_str::<Message>(r#"{"foo":"bar"}"#).is_err());

        // A payload that carries at least one response field is still a
        // response, even when `result` and `error` are omitted.
        match serde_json::from_str::<Message>(r#"{"id":1}"#).unwrap() {
            Message::Response(response) => {
                assert_eq!(response.id, RequestId::Number(1));
                assert!(response.result.is_none());
                assert!(response.error.is_none());
            }
            other => panic!("Expected Response, got {other:?}"),
        }

        // A response that carries only a result (no id, no error) is accepted.
        match serde_json::from_str::<Message>(r#"{"result":{"ok":true}}"#).unwrap() {
            Message::Response(response) => {
                assert_eq!(response.id, RequestId::Null);
                assert!(response.result.is_some());
            }
            other => panic!("Expected Response, got {other:?}"),
        }
    }

    #[test]
    fn test_request_debug_redacts_credentials_save() {
        let req = Request {
            id: RequestId::Number(1),
            method: crate::methods::CREDENTIALS_SAVE.to_string(),
            params: Some(serde_json::json!({
                "api_key": "sk-secret-12345"
            })),
        };

        let rendered = format!("{req:?}");
        assert!(!rendered.contains("sk-secret-12345"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn test_message_debug_redacts_credentials_save() {
        let msg = Message::Request(Request {
            id: RequestId::Number(1),
            method: crate::methods::CREDENTIALS_SAVE.to_string(),
            params: Some(serde_json::json!({
                "api_key": "sk-secret-67890"
            })),
        });

        let rendered = format!("{msg:?}");
        assert!(!rendered.contains("sk-secret-67890"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn test_request_debug_preserves_non_sensitive_params() {
        let req = Request {
            id: RequestId::Number(2),
            method: crate::methods::CONTEXT_ADD.to_string(),
            params: Some(serde_json::json!({
                "path": "crates/protocol/src/lib.rs"
            })),
        };

        let rendered = format!("{req:?}");
        assert!(rendered.contains("crates/protocol/src/lib.rs"));
    }

    #[test]
    fn test_request_debug_redacts_sensitive_keys_even_in_custom_methods() {
        let req = Request {
            id: RequestId::Number(3),
            method: "custom/auth".to_string(),
            params: Some(serde_json::json!({
                "api_key": "sk-secret-custom",
                "normal_field": "visible_value"
            })),
        };

        let rendered = format!("{req:?}");
        assert!(!rendered.contains("sk-secret-custom"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("visible_value"));
    }

    #[test]
    fn test_response_debug_redacts_sensitive_keys() {
        let resp = Response::ok(
            RequestId::Number(4),
            serde_json::json!({
                "api_key": "sk-secret-response",
                "status": "ok"
            }),
        );

        let rendered = format!("{resp:?}");
        assert!(!rendered.contains("sk-secret-response"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("ok"));
    }

    #[test]
    fn test_response_error_debug_redacts_sensitive_keys() {
        let err = ResponseError::with_data(
            "AUTH_FAILED",
            "invalid token",
            serde_json::json!({
                "token": "secret-token-failed"
            }),
        );

        let rendered = format!("{err:?}");
        assert!(!rendered.contains("secret-token-failed"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn test_is_sensitive_key_matches_credential_variants() {
        for key in [
            "api_key",
            "api-key",
            "apiKey",
            "apikey",
            "key",
            "openrouter_api_key",
            "access_token",
            "refresh_token",
            "id_token",
            "secret",
            "client_secret",
            "password",
            "new_password",
            "credential",
            "credentials",
            "authorization",
            "bearer",
            "api_keys",
            "ssh_keys",
            "access_tokens",
            "refresh_tokens",
            "id_tokens",
        ] {
            assert!(is_sensitive_key(key), "{key} must be treated as sensitive");
        }

        for key in ["path", "method", "model", "name", "keyword", "count"] {
            assert!(
                !is_sensitive_key(key),
                "{key} must not be treated as sensitive"
            );
        }

        // Usage metric names sit inside the `token` credential family, so
        // name-level matching marks them sensitive. They escape redaction only
        // through the value-level exemption in `redact_sensitive_json`, which
        // requires a numeric counter.
        for key in [
            "tokens",
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "reasoning_tokens",
            "cached_tokens",
            "estimated_tokens",
            "added_tokens",
        ] {
            assert!(
                is_token_metric_key(&normalize_key(key)),
                "{key} must stay on the metric allowlist"
            );
            assert!(
                is_sensitive_key(key),
                "{key} must match the token credential family"
            );
        }
    }

    #[test]
    fn test_debug_redacts_nested_objects_and_arrays_under_sensitive_key() {
        let req = Request {
            id: RequestId::Number(10),
            method: "custom/auth".to_string(),
            params: Some(serde_json::json!({
                "api_key": {
                    "value": "sk-secret-nested",
                    "backup": ["sk-secret-arr-1", "sk-secret-arr-2"]
                },
                "credentials": [
                    {"token": "nested-token-1"},
                    {"value": "nested-val-2"}
                ]
            })),
        };

        let rendered = format!("{req:?}");
        assert!(!rendered.contains("sk-secret-nested"));
        assert!(!rendered.contains("sk-secret-arr-1"));
        assert!(!rendered.contains("sk-secret-arr-2"));
        assert!(!rendered.contains("nested-token-1"));
        assert!(!rendered.contains("nested-val-2"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn test_debug_preserves_token_metrics_in_response_and_event() {
        let resp = Response::ok(
            RequestId::Number(20),
            serde_json::json!({
                "estimated_tokens": 1500,
                "status": "ok"
            }),
        );
        let resp_rendered = format!("{resp:?}");
        assert!(resp_rendered.contains("1500"));
        assert!(resp_rendered.contains("estimated_tokens"));

        let event = Event {
            method: "model/usage".to_string(),
            params: Some(serde_json::json!({
                "prompt_tokens": 120,
                "completion_tokens": 45,
                "total_tokens": 165
            })),
        };
        let event_rendered = format!("{event:?}");
        assert!(event_rendered.contains("120"));
        assert!(event_rendered.contains("45"));
        assert!(event_rendered.contains("165"));
        assert!(event_rendered.contains("prompt_tokens"));
    }

    #[test]
    fn test_request_debug_redacts_common_credential_key_variants() {
        let req = Request {
            id: RequestId::Number(5),
            method: "custom/echo".to_string(),
            params: Some(serde_json::json!({
                "openrouter_api_key": "sk-or-secret-value",
                "access_token": "access-secret-value",
                "refresh_token": "refresh-secret-value",
                "authorization": "auth-secret-value",
                "client_secret": "client-secret-value",
                "bearer": "bearer-secret-value",
                "api-key": "dashed-secret-value",
                "safe_field": "visible_value"
            })),
        };

        let rendered = format!("{req:?}");
        for secret in [
            "sk-or-secret-value",
            "access-secret-value",
            "refresh-secret-value",
            "auth-secret-value",
            "client-secret-value",
            "bearer-secret-value",
            "dashed-secret-value",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked into Debug");
        }
        assert!(rendered.contains("visible_value"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn test_debug_redacts_plural_credential_keys() {
        let req = Request {
            id: RequestId::Number(6),
            method: "custom/echo".to_string(),
            params: Some(serde_json::json!({
                "access_tokens": "access-token-value",
                "refresh_tokens": "refresh-token-value",
                "id_tokens": "id-token-value",
                "api_keys": "api-key-value",
                "ssh_keys": "ssh-key-value",
                "estimated_tokens": 2048
            })),
        };

        let rendered = format!("{req:?}");
        for secret in [
            "access-token-value",
            "refresh-token-value",
            "id-token-value",
            "api-key-value",
            "ssh-key-value",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked into Debug");
        }
        assert!(rendered.contains("<redacted>"));

        // An allowlisted usage metric stays visible only while it binds a
        // numeric counter.
        assert!(rendered.contains("2048"));
        assert!(rendered.contains("estimated_tokens"));
    }

    #[test]
    fn test_debug_redacts_non_numeric_values_under_metric_keys() {
        let req = Request {
            id: RequestId::Number(7),
            method: "custom/echo".to_string(),
            params: Some(serde_json::json!({
                "tokens": "sk-live-string-secret",
                "estimated_tokens": ["sk-live-array-secret"],
                "prompt_tokens": {"value": "sk-live-object-secret"},
                "total_tokens": 4096
            })),
        };

        let rendered = format!("{req:?}");
        for secret in [
            "sk-live-string-secret",
            "sk-live-array-secret",
            "sk-live-object-secret",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked into Debug");
        }
        assert!(rendered.contains("<redacted>"));

        // The same key with a numeric counter keeps its value readable, so the
        // exemption stays useful for usage metrics.
        assert!(rendered.contains("4096"));
        assert!(rendered.contains("total_tokens"));
    }

    #[test]
    fn test_debug_redacts_non_numeric_values_under_metric_keys_in_event() {
        let event = Event {
            method: "model/usage".to_string(),
            params: Some(serde_json::json!({
                "tokens": "sk-live-event-secret",
                "completion_tokens": 45
            })),
        };

        let rendered = format!("{event:?}");
        assert!(!rendered.contains("sk-live-event-secret"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("45"));
    }
}
