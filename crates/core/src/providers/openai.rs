//! Responses API transport for the user's authorized ChatGPT plan.
pub(super) mod login;
mod session;
use session::{access_token, TOKEN_ENDPOINT};
use std::hash::{BuildHasher, RandomState};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use super::sse::{find_sse_event_boundary, MAX_SSE_EVENT_BYTES};
use super::{
    LlmProvider, LlmRequest, ModelUsageInfo, ProviderError, ProviderResult, ResponseFormat,
    StreamEvent,
};
use crate::models::ProviderKind;

const ENDPOINT: &str = "https://api.openai.com/v1/responses";

pub(super) struct OpenAiProvider {
    client: Client,
    credentials: PathBuf,
    sign_in: Option<std::sync::Arc<login::ChatGptSignIn>>,
}

fn request_body(request: LlmRequest<'_>) -> Value {
    let input: Vec<_> = request
        .messages
        .into_iter()
        .map(|message| {
            let role = if message.role == "system" {
                "developer"
            } else {
                &message.role
            };
            json!({"role": role, "content": message.content})
        })
        .collect();
    let mut body = json!({"model":request.model,"input":input,"store":false,"stream":true});
    body["reasoning"] = json!({"summary":"auto"});
    if let Some(effort) = request.reasoning_effort {
        body["reasoning"]["effort"] = json!(effort.to_string());
    }
    if let Some(format) = request.response_format {
        let format = match format {
            ResponseFormat::Text => json!({"type":"text"}),
            ResponseFormat::JsonObject => json!({"type":"json_object"}),
            ResponseFormat::JsonSchema { json_schema } => {
                let mut format = json!(json_schema);
                format["type"] = json!("json_schema");
                format
            }
        };
        body["text"] = json!({"format":format});
    }
    body
}
static CACHE_PROBE: LazyLock<Option<Mutex<CacheProbe>>> = LazyLock::new(|| {
    std::env::var("TAUQE_DEV_OPENAI_CACHE")
        .is_ok_and(|value| value == "1")
        .then(|| Mutex::new(CacheProbe::default()))
});

/// Retain only the previous request's fingerprints and lengths across provider instances.
#[derive(Default)]
struct CacheProbe {
    hasher: RandomState,
    previous: Option<RequestShape>,
    request_id: u64,
}

struct RequestShape {
    model: String,
    messages: Vec<(u64, usize)>,
    format: Option<u64>,
}

impl RequestShape {
    fn from_body(body: &Value, hasher: &RandomState) -> Self {
        let messages = body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|message| {
                let role = message["role"].as_str().unwrap_or_default();
                let content = message["content"].as_str().unwrap_or_default();
                (hasher.hash_one((role, content)), content.len())
            })
            .collect();
        Self {
            model: body["model"].as_str().unwrap_or_default().to_owned(),
            messages,
            format: body["text"]
                .get("format")
                .map(|format| hasher.hash_one(format.to_string())),
        }
    }

    fn common_prefix(&self, previous: &Self) -> (usize, usize) {
        self.messages
            .iter()
            .zip(&previous.messages)
            .take_while(|(current, previous)| current == previous)
            .fold((0, 0), |(count, bytes), ((_, size), _)| {
                (count + 1, bytes + size)
            })
    }
}

impl CacheProbe {
    fn record(&mut self, body: &Value) -> u64 {
        let current = RequestShape::from_body(body, &self.hasher);
        self.request_id += 1;
        let request_id = self.request_id;
        let previous = self.previous.as_ref().filter(|p| p.model == current.model);
        let common = previous.map(|p| current.common_prefix(p));
        let first_changed_index = previous
            .zip(common)
            .and_then(|(p, (count, _))| (count < p.messages.len()).then_some(count));
        tracing::info!(
            request_id,
            model = %current.model,
            input_messages = current.messages.len(),
            assistant_messages = body["input"].as_array().map(|items| items.iter().filter(|item| item["role"] == "assistant").count()),
            previous_input_messages = ?previous.map(|p| p.messages.len()),
            common_prefix_messages = ?common.map(|(count, _)| count),
            common_prefix_content_bytes = ?common.map(|(_, bytes)| bytes),
            first_changed_index = ?first_changed_index,
            format_fingerprint = ?current.format,
            format_changed = ?previous.map(|p| p.format != current.format),
            "OpenAI prompt cache request"
        );
        self.previous = Some(current);
        request_id
    }
}
#[derive(Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    total_tokens: u32,
    input_tokens_details: Option<InputDetails>,
    output_tokens_details: Option<OutputDetails>,
}
#[derive(Deserialize)]
struct InputDetails {
    cached_tokens: Option<u32>,
    cache_write_tokens: Option<u32>,
}
#[derive(Deserialize)]
struct OutputDetails {
    reasoning_tokens: Option<u32>,
}

fn response_error(status: Option<StatusCode>, error: &Value) -> ProviderError {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    let message = error
        .get("message")
        .or_else(|| error.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("Request failed");
    let detail = format!(
        "OpenAI {} ({code}): {message}",
        status.map_or_else(|| "stream error".into(), |status| status.to_string())
    );
    match code {
        "subscription_sharing_usage_limit_exceeded" => ProviderError::PlanLimitReached(detail),
        "subscription_sharing_user_unavailable" | "subscription_sharing_usage_unavailable" => {
            ProviderError::Transient(anyhow::anyhow!(detail))
        }
        _ if status == Some(StatusCode::UNAUTHORIZED) => {
            ProviderError::AuthenticationRequired(ProviderKind::OpenAI)
        }
        _ if status.is_some_and(|s| {
            s.is_server_error()
                || s == StatusCode::TOO_MANY_REQUESTS
                || s == StatusCode::REQUEST_TIMEOUT
        }) || code == "server_error" =>
        {
            ProviderError::Transient(anyhow::anyhow!(detail))
        }
        _ => ProviderError::Request(anyhow::anyhow!(detail)),
    }
}

async fn handle_event(
    frame: &[u8],
    tx: &mpsc::Sender<StreamEvent>,
    cache_probe_request: Option<u64>,
) -> Result<bool, ProviderError> {
    let text = std::str::from_utf8(frame).map_err(|e| ProviderError::Transient(e.into()))?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(false);
    }
    let event: Value =
        serde_json::from_str(&data).map_err(|e| ProviderError::Transient(e.into()))?;
    match event["type"].as_str() {
        Some(kind @ ("response.output_text.delta" | "response.reasoning_summary_text.delta")) => {
            let delta = event["delta"]
                .as_str()
                .ok_or_else(|| {
                    ProviderError::Request(anyhow::anyhow!("OpenAI delta is missing text"))
                })?
                .to_owned();
            let update = if kind == "response.output_text.delta" {
                StreamEvent::TextDelta(delta)
            } else {
                StreamEvent::ReasoningDelta(delta)
            };
            if tx.send(update).await.is_err() {
                return Ok(true);
            }
        }
        Some("response.completed") => {
            let response = &event["response"];
            if let Some(value) = response.get("usage").filter(|v| !v.is_null()) {
                let usage: Usage = serde_json::from_value(value.clone())
                    .map_err(|e| ProviderError::Request(e.into()))?;
                let input_details = usage.input_tokens_details.as_ref();
                let cached_tokens = input_details.and_then(|d| d.cached_tokens);
                if let Some(request_id) = cache_probe_request {
                    tracing::info!(
                        request_id,
                        input_tokens = usage.input_tokens,
                        ?cached_tokens,
                        cache_write_tokens = ?input_details.and_then(|d| d.cache_write_tokens),
                        effective_model = ?response["model"].as_str(),
                        effective_service_tier = ?response["service_tier"].as_str(),
                        effective_reasoning_effort = ?response["reasoning"]["effort"].as_str(),
                        "OpenAI prompt cache usage"
                    );
                }
                let _ = tx
                    .send(StreamEvent::Usage(ModelUsageInfo {
                        prompt_tokens: usage.input_tokens,
                        completion_tokens: usage.output_tokens,
                        total_tokens: usage.total_tokens,
                        cached_tokens,
                        reasoning_tokens: usage
                            .output_tokens_details
                            .and_then(|d| d.reasoning_tokens),
                        cost: None,
                    }))
                    .await;
            }
            let _ = tx.send(StreamEvent::Done).await;
            return Ok(true);
        }
        Some("response.failed") => return Err(response_error(None, &event["response"]["error"])),
        Some("error") => return Err(response_error(None, event.get("error").unwrap_or(&event))),
        Some("response.incomplete") => {
            return Err(ProviderError::Request(anyhow::anyhow!(
                "OpenAI response incomplete: {}",
                event["response"]["incomplete_details"]["reason"]
            )))
        }
        _ => (),
    }
    Ok(false)
}

impl OpenAiProvider {
    pub(super) fn new(
        credentials: PathBuf,
        sign_in: Option<std::sync::Arc<login::ChatGptSignIn>>,
    ) -> anyhow::Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            credentials,
            sign_in,
        })
    }

    async fn execute(
        &self,
        url: &str,
        request: LlmRequest<'_>,
        tx: &mpsc::Sender<StreamEvent>,
    ) -> ProviderResult {
        let model = request.model;
        let body = request_body(request);
        match self.send(url, &body, tx).await {
            Err(error @ ProviderError::AuthenticationRequired(_)) => {
                let Some(login) = &self.sign_in else {
                    return Err(error);
                };
                if login
                    .authorize(&self.credentials, model, &self.client)
                    .await
                    .map_err(ProviderError::Request)?
                {
                    self.send(url, &body, tx).await
                } else {
                    let _ = tx.send(StreamEvent::Cancelled).await;
                    Ok(())
                }
            }
            result => result,
        }
    }

    async fn send(
        &self,
        url: &str,
        body: &Value,
        tx: &mpsc::Sender<StreamEvent>,
    ) -> ProviderResult {
        let token = access_token(&self.credentials, &self.client, TOKEN_ENDPOINT).await?;
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::Transient(e.into()))?;
        if !response.status().is_success() {
            let status = response.status();
            let error = response.json::<Value>().await.unwrap_or(Value::Null);
            return Err(response_error(
                Some(status),
                error.get("error").unwrap_or(&error),
            ));
        }
        let cache_probe_request = CACHE_PROBE
            .as_ref()
            .and_then(|probe| probe.lock().ok())
            .map(|mut probe| probe.record(body));
        let mut bytes = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut from = 0;
        while let Some(chunk) = bytes.next().await {
            buffer.extend_from_slice(&chunk.map_err(|e| ProviderError::Transient(e.into()))?);
            let mut consumed = 0;
            while let Some((end, delimiter)) = find_sse_event_boundary(&buffer[consumed..], from) {
                if end > MAX_SSE_EVENT_BYTES {
                    return Err(oversized());
                }
                if handle_event(&buffer[consumed..consumed + end], tx, cache_probe_request).await? {
                    return Ok(());
                }
                consumed += end + delimiter;
                from = 0;
            }
            buffer.drain(..consumed);
            if buffer.len() > MAX_SSE_EVENT_BYTES {
                return Err(oversized());
            }
            from = buffer.len().saturating_sub(3);
        }
        Err(ProviderError::Transient(anyhow::anyhow!(
            "OpenAI stream ended before response.completed"
        )))
    }
}
fn oversized() -> ProviderError {
    ProviderError::Request(anyhow::anyhow!("OpenAI stream event exceeds size limit"))
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAI
    }
    async fn stream(
        &self,
        request: LlmRequest<'_>,
        tx: mpsc::Sender<StreamEvent>,
        mut cancel: watch::Receiver<bool>,
    ) -> ProviderResult {
        let cancelled = async {
            if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            _ = cancelled => {let _ = tx.send(StreamEvent::Cancelled).await; Ok(())}
            _ = tx.closed() => Ok(()),
            result = self.execute(ENDPOINT, request, &tx) => result,
        }
    }
}

#[cfg(test)]
mod cache_live_probe;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ChatMessage, JsonSchemaDefinition};
    use std::borrow::Cow;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn reasoning_summary_preserves_explicit_effort() {
        let request = LlmRequest {
            reasoning_effort: Some(crate::models::ReasoningEffort::High),
            ..LlmRequest::new("test-model", Vec::new())
        };
        assert_eq!(
            request_body(request)["reasoning"],
            json!({"summary":"auto","effort":"high"})
        );
    }
    #[test]
    fn cache_probe_compares_ordered_messages_and_response_formats() {
        let hasher = RandomState::new();
        let mut body = request_body(LlmRequest::new(
            "test-model",
            vec![ChatMessage::system("contract"), ChatMessage::user("task")],
        ));
        body["text"] = json!({"format": {"type": "json_schema", "name": "first"}});
        let original = RequestShape::from_body(&body, &hasher);

        let mut body = request_body(LlmRequest::new(
            "test-model",
            vec![
                ChatMessage::system("contract"),
                ChatMessage::user("task"),
                ChatMessage::assistant("Loading the first pair."),
                ChatMessage::user("feedback"),
            ],
        ));
        body["text"] = json!({"format": {"type": "json_schema", "name": "first"}});
        assert_eq!(
            body["input"][2],
            json!({"role": "assistant", "content": "Loading the first pair."})
        );
        let extended = RequestShape::from_body(&body, &hasher);
        assert_eq!(extended.common_prefix(&original), (2, 12));
        assert_eq!(extended.format, original.format);

        body["input"][1]["role"] = json!("assistant");
        body["text"]["format"]["name"] = json!("second");
        let changed = RequestShape::from_body(&body, &hasher);
        assert_eq!(changed.common_prefix(&original), (1, 8));
        assert_ne!(changed.format, original.format);
    }
    #[test]
    fn cache_usage_distinguishes_missing_writes_from_zero() {
        let missing: InputDetails = serde_json::from_value(json!({"cached_tokens": 4})).unwrap();
        let zero: InputDetails =
            serde_json::from_value(json!({"cached_tokens": 4, "cache_write_tokens": 0})).unwrap();
        let written: InputDetails =
            serde_json::from_value(json!({"cached_tokens": 4, "cache_write_tokens": 8})).unwrap();

        assert_eq!(missing.cache_write_tokens, None);
        assert_eq!(zero.cache_write_tokens, Some(0));
        assert_eq!(written.cache_write_tokens, Some(8));
        assert_eq!(written.cached_tokens, Some(4));
    }

    #[tokio::test]
    async fn responses_request_streams_reasoning_text_and_subscription_usage() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("credentials.toml");
        tokio::fs::write(
            &credentials,
            r#"
[openai.chatgpt]
active_client_id = "client"
[[openai.chatgpt.sessions]]
client_id = "client"
access_token = "test-token"
expires_at = 4102444800
scopes = ["chatgpt.tokens.use.direct"]
"#,
        )
        .await
        .unwrap();
        let provider = OpenAiProvider::new(credentials, None).unwrap();
        let server = MockServer::start().await;
        let response = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Summary\"}\n\n",
            "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\r\n\r\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":2,\"total_tokens\":14,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n\n"
        );
        Mock::given(method("POST")).and(path("/v1/responses")).and(header("authorization","Bearer test-token"))
            .and(body_partial_json(json!({"store":false,"stream":true,"model":"test-model","reasoning":{"summary":"auto"},"input":[{"role":"developer","content":"instructions"},{"role":"user","content":"question"}],"text":{"format":{"type":"json_schema","name":"answer","strict":true,"schema":{"type":"object"}}}})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(response,"text/event-stream")).expect(1).mount(&server).await;
        let request = LlmRequest {
            response_format: Some(ResponseFormat::JsonSchema {
                json_schema: JsonSchemaDefinition {
                    name: "answer".into(),
                    description: None,
                    schema: Cow::Owned(json!({"type":"object"})),
                    strict: Some(true),
                },
            }),
            ..LlmRequest::new(
                "test-model",
                vec![
                    ChatMessage::system("instructions"),
                    ChatMessage::user("question"),
                ],
            )
        };
        let (tx, mut rx) = mpsc::channel(4);
        provider
            .execute(&format!("{}/v1/responses", server.uri()), request, &tx)
            .await
            .unwrap();
        assert!(matches!(rx.recv().await,Some(StreamEvent::ReasoningDelta(s)) if s == "Summary"));
        assert!(matches!(rx.recv().await,Some(StreamEvent::TextDelta(s)) if s == "OK"));
        assert!(
            matches!(rx.recv().await,Some(StreamEvent::Usage(u)) if u.cost.is_none() && u.total_tokens == 14 && u.cached_tokens == Some(4))
        );
        assert!(matches!(rx.recv().await, Some(StreamEvent::Done)));
    }

    #[tokio::test]
    async fn missing_session_requests_authentication_and_streamed_plan_limit_is_terminal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            access_token(&dir.path().join("missing"), &Client::new(), TOKEN_ENDPOINT).await,
            Err(ProviderError::AuthenticationRequired(ProviderKind::OpenAI))
        ));
        let (tx, _) = mpsc::channel(1);
        let frame = br#"data: {"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}"#;
        assert!(matches!(
            handle_event(frame, &tx, None).await,
            Err(ProviderError::PlanLimitReached(_))
        ));
    }
}
