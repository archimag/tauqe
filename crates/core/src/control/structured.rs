//! Read-only structured requests without repository context or edit policy.

use crate::edits::stream::json::JsonFieldStreamFilter;
use crate::edits::stream::EditStreamFilter;
use crate::models::ReasoningEffort;
use crate::providers::{ChatMessage, LlmProvider, LlmRequest, ResponseFormat, StreamEvent};

use super::context::{CancellationToken, EventSink};
use super::request;

/// One frozen model-facing contract for a typed read-only request.
pub struct StructuredContract<T> {
    pub instructions: String,
    pub format: ResponseFormat,
    pub decode: fn(&str) -> Result<T, String>,
    pub visibility: StructuredVisibility,
    /// Whether a provider close without Done invalidates the payload.
    pub require_done: bool,
}

#[derive(Clone, Copy)]
pub enum StructuredVisibility {
    Silent,
    /// Publish only the decoded contents of the selected JSON string field.
    TextField(&'static str),
}

struct StructuredFilter {
    visibility: StructuredVisibility,
    field: Option<JsonFieldStreamFilter<serde_json::Value>>,
}

impl StructuredFilter {
    fn new(visibility: StructuredVisibility) -> Self {
        Self {
            visibility,
            field: match visibility {
                StructuredVisibility::Silent => None,
                StructuredVisibility::TextField(name) => Some(JsonFieldStreamFilter::new(name)),
            },
        }
    }
}

impl EditStreamFilter for StructuredFilter {
    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.field
            .as_mut()
            .and_then(|field| field.push_chunk(chunk))
            .map(StreamEvent::TextDelta)
            .into_iter()
            .collect()
    }

    fn finish(self: Box<Self>) -> Vec<StreamEvent> {
        Vec::new()
    }

    fn reset(&mut self) {
        *self = Self::new(self.visibility);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StructuredError {
    #[error("Structured request was cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
    #[error("Structured request returned an empty payload")]
    Empty,
    #[error("Structured request closed without Done")]
    Incomplete,
    #[error("{0}")]
    Decode(String),
}

/// Executes one request; domain validation and further rounds belong to the caller.
#[allow(clippy::too_many_arguments)]
pub async fn execute<T>(
    provider: &dyn LlmProvider,
    model: &str,
    effort: Option<ReasoningEffort>,
    session: Option<&str>,
    contract: &StructuredContract<T>,
    messages: Vec<ChatMessage>,
    events: &EventSink,
    cancel: &CancellationToken,
) -> Result<T, StructuredError> {
    if cancel.is_cancelled() {
        return Err(StructuredError::Cancelled);
    }
    let mut request_messages = vec![ChatMessage::system(&contract.instructions)];
    request_messages.extend(messages);
    let stream = request::execute(
        provider,
        LlmRequest {
            model,
            messages: request_messages,
            response_format: Some(contract.format.clone()),
            reasoning_effort: effort,
            session_id: session,
        },
        Box::new(StructuredFilter::new(contract.visibility)),
        events,
        cancel.receiver(),
    )
    .await;
    if cancel.is_cancelled() {
        return Err(StructuredError::Cancelled);
    }
    let stream = stream.map_err(|error| StructuredError::Failed(error.to_string()))?;
    if stream.cancelled {
        return Err(StructuredError::Cancelled);
    }
    if let Some(error) = stream.stream_error {
        return Err(StructuredError::Failed(error));
    }
    if contract.require_done && !stream.completed {
        return Err(StructuredError::Incomplete);
    }
    if stream.text.trim().is_empty() {
        return Err(StructuredError::Empty);
    }
    (contract.decode)(&stream.text).map_err(StructuredError::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::providers::JsonSchemaDefinition;
    use std::borrow::Cow;

    #[tokio::test]
    async fn silent_request_decodes_without_publishing_payload() {
        let provider = MockLlmProvider::scripted([MockResponse::text("{\"ok\":true}")]);
        let contract = StructuredContract {
            instructions: "Return a result".into(),
            format: ResponseFormat::JsonSchema {
                json_schema: JsonSchemaDefinition {
                    name: "result".into(),
                    description: None,
                    schema: Cow::Owned(serde_json::json!({"type":"object"})),
                    strict: Some(true),
                },
            },
            decode: |text| {
                serde_json::from_str::<serde_json::Value>(text).map_err(|error| error.to_string())
            },
            visibility: StructuredVisibility::Silent,
            require_done: true,
        };
        let (sink, mut rx) = EventSink::channel();
        let result = execute(
            &provider,
            "test",
            None,
            None,
            &contract,
            vec![ChatMessage::user("Task")],
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result["ok"], true);
        assert!(!std::iter::from_fn(|| rx.try_recv().ok())
            .any(|event| matches!(event, StreamEvent::TextDelta(_))));
        let request = provider.last_request().unwrap();
        assert_eq!(request.messages[0].content, "Return a result");
        assert_eq!(request.messages[1].content, "Task");
        assert_eq!(request.response_format, Some(contract.format));
    }

    struct Script(Vec<StreamEvent>);

    #[async_trait::async_trait]
    impl LlmProvider for Script {
        fn kind(&self) -> crate::models::ProviderKind {
            crate::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _: LlmRequest<'_>,
            tx: tokio::sync::mpsc::Sender<StreamEvent>,
            _: tokio::sync::watch::Receiver<bool>,
        ) -> crate::providers::ProviderResult {
            for event in &self.0 {
                tx.send(event.clone()).await.unwrap();
            }
            Ok(())
        }
    }

    fn contract(
        visibility: StructuredVisibility,
        require_done: bool,
    ) -> StructuredContract<serde_json::Value> {
        StructuredContract {
            instructions: "Return JSON".into(),
            format: ResponseFormat::JsonObject,
            decode: |text| serde_json::from_str(text).map_err(|e| e.to_string()),
            visibility,
            require_done,
        }
    }

    async fn run<T>(
        script: Vec<StreamEvent>,
        contract: &StructuredContract<T>,
    ) -> (Result<T, StructuredError>, Vec<StreamEvent>) {
        let (sink, mut rx) = EventSink::channel();
        let result = execute(
            &Script(script),
            "test",
            None,
            None,
            contract,
            vec![],
            &sink,
            &CancellationToken::new(),
        )
        .await;
        (result, std::iter::from_fn(|| rx.try_recv().ok()).collect())
    }

    #[tokio::test]
    async fn completion_policy_and_empty_payload_are_distinct() {
        let payload = StreamEvent::TextDelta("{\"ok\":true}".into());
        let strict = contract(StructuredVisibility::Silent, true);
        assert!(run(vec![payload.clone(), StreamEvent::Done], &strict)
            .await
            .0
            .is_ok());
        assert!(matches!(
            run(vec![payload.clone()], &strict).await.0,
            Err(StructuredError::Incomplete)
        ));
        let permissive = contract(StructuredVisibility::Silent, false);
        assert!(run(vec![payload], &permissive).await.0.is_ok());
        assert!(matches!(
            run(vec![StreamEvent::Done], &strict).await.0,
            Err(StructuredError::Empty)
        ));
        assert!(matches!(
            run(
                vec![StreamEvent::TextDelta("invalid".into()), StreamEvent::Done],
                &strict
            )
            .await
            .0,
            Err(StructuredError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn error_after_done_and_cancellation_override_decoding() {
        let strict = contract(StructuredVisibility::Silent, true);
        assert!(
            matches!(run(vec![StreamEvent::TextDelta("{}".into()), StreamEvent::Done, StreamEvent::Error("late".into())], &strict).await.0, Err(StructuredError::Failed(message)) if message == "late")
        );
        assert!(matches!(
            run(vec![StreamEvent::Cancelled], &strict).await.0,
            Err(StructuredError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn field_filter_decodes_text_without_exposing_json_and_resets_payload_only() {
        let contract = contract(StructuredVisibility::TextField("message"), true);
        let (result, events) = run(
            vec![
                StreamEvent::Usage(crate::providers::ModelUsageInfo {
                    prompt_tokens: 5,
                    ..Default::default()
                }),
                StreamEvent::TextDelta("{\"message\":\"discard\"}".into()),
                StreamEvent::ResetStream,
                StreamEvent::TextDelta("{\"message\":\"Hi \\uD83D\\uDE00\",\"private\":7}".into()),
                StreamEvent::Done,
            ],
            &contract,
        )
        .await;
        assert_eq!(result.unwrap()["private"], 7);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "discardHi 😀");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::Usage(_)))
                .count(),
            1
        );
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ResetStream)));
    }

    #[tokio::test]
    async fn preview_is_not_a_success_when_complete_json_fails_schema_decoding() {
        let contract = StructuredContract {
            instructions: "Return JSON".into(),
            format: ResponseFormat::JsonObject,
            decode: |text| {
                let value: serde_json::Value =
                    serde_json::from_str(text).map_err(|error| error.to_string())?;
                value["count"]
                    .as_u64()
                    .ok_or_else(|| "count must be a number".to_string())
            },
            visibility: StructuredVisibility::TextField("message"),
            require_done: true,
        };
        let (result, events) = run(
            vec![
                StreamEvent::TextDelta(r#"{"message":"Early","count":"bad"}"#.into()),
                StreamEvent::Done,
            ],
            &contract,
        )
        .await;
        assert!(
            matches!(result, Err(StructuredError::Decode(reason)) if reason == "count must be a number")
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "Early")));
    }
}
