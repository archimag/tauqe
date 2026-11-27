//! Admission of the complete model input before a provider is called.
//!
//! The estimate is `ceil(serialized_request_bytes / 4)`, using the shared
//! substrate heuristic. JSON framing, roles, escaped message content, schema,
//! and adapter-added fields count. This is **not** the provider's tokenizer
//! count: hidden prefixes and server-side transformations cannot be observed.
//! Admission never truncates materials to make a request fit.

use crate::models::ProviderKind;
use crate::providers::LlmRequest;
use serde_json::Value;
use tauqe_tools::tokens::estimate_tokens_from_bytes;

use super::outcome::ExecutionError;

/// Diagnostic sizes without any model-visible content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSize {
    pub total: usize,
    pub messages: usize,
    pub response_format: usize,
    pub adapter_and_framing: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("estimated model input {size} tokens exceeds limit {limit} tokens (messages: {messages}, response format: {response_format}, adapter and framing: {adapter_and_framing})")]
pub struct InputLimitExceeded {
    pub size: usize,
    pub limit: usize,
    pub messages: usize,
    pub response_format: usize,
    pub adapter_and_framing: usize,
}

fn bytes(value: &Value) -> Result<usize, ExecutionError> {
    serde_json::to_vec(value)
        .map(|encoded| encoded.len())
        .map_err(|error| ExecutionError::Other(format!("cannot measure model input: {error}")))
}

/// Components are rounded individually for diagnostics; admission rounds the
/// full serialized body once, so diagnostic components need not sum to total.
pub fn measure(
    request: &LlmRequest<'_>,
    provider: ProviderKind,
) -> Result<InputSize, ExecutionError> {
    let body = match provider {
        ProviderKind::OpenAI => crate::providers::openai::input_projection(request),
        ProviderKind::OpenRouter => crate::providers::openrouter::input_projection(request)
            .map_err(|error| {
                ExecutionError::Other(format!("cannot measure model input: {error}"))
            })?,
    };
    let total_bytes = bytes(&body)?;
    let messages_bytes = match provider {
        ProviderKind::OpenAI => bytes(&body["input"])?,
        ProviderKind::OpenRouter => bytes(&body["messages"])?,
    };
    let format_bytes = match provider {
        ProviderKind::OpenAI => body.get("text").map(bytes).transpose()?.unwrap_or(0),
        ProviderKind::OpenRouter => body
            .get("response_format")
            .map(bytes)
            .transpose()?
            .unwrap_or(0),
    };
    let estimate = |n: usize| estimate_tokens_from_bytes(n as u64) as usize;
    Ok(InputSize {
        total: estimate(total_bytes),
        messages: estimate(messages_bytes),
        response_format: estimate(format_bytes),
        adapter_and_framing: estimate(total_bytes.saturating_sub(messages_bytes + format_bytes)),
    })
}

pub fn admit(
    request: &LlmRequest<'_>,
    provider: ProviderKind,
    limit: usize,
) -> Result<InputSize, ExecutionError> {
    let size = measure(request, provider)?;
    if size.total > limit {
        return Err(InputLimitExceeded {
            size: size.total,
            limit,
            messages: size.messages,
            response_format: size.response_format,
            adapter_and_framing: size.adapter_and_framing,
        }
        .into());
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ChatMessage, JsonSchemaDefinition, ResponseFormat};
    use std::borrow::Cow;

    #[test]
    fn boundary_and_schema_count_on_complete_request() {
        let mut request = LlmRequest::new("test", vec![ChatMessage::user("task")]);
        let plain = measure(&request, ProviderKind::OpenAI).unwrap();
        assert!(admit(&request, ProviderKind::OpenAI, plain.total + 1).is_ok());
        assert!(admit(&request, ProviderKind::OpenAI, plain.total).is_ok());
        assert!(matches!(
            admit(&request, ProviderKind::OpenAI, plain.total - 1),
            Err(ExecutionError::InputLimit(error)) if error.size == plain.total
        ));
        request.response_format = Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "answer".into(),
                description: None,
                schema: Cow::Owned(
                    serde_json::json!({"type":"string","description":"x".repeat(120)}),
                ),
                strict: Some(true),
            },
        });
        let with_schema = measure(&request, ProviderKind::OpenAI).unwrap();
        assert!(with_schema.total > plain.total);
        assert!(admit(&request, ProviderKind::OpenAI, plain.total).is_err());
        assert!(with_schema.response_format > 0);
    }

    #[test]
    fn feedback_and_adapter_fields_count() {
        let mut request = LlmRequest::new("test", vec![ChatMessage::user("task")]);
        let before = measure(&request, ProviderKind::OpenRouter).unwrap();
        request
            .messages
            .push(ChatMessage::user("feedback".repeat(50)));
        let after = measure(&request, ProviderKind::OpenRouter).unwrap();
        assert!(after.total > before.total);
        assert!(admit(&request, ProviderKind::OpenRouter, before.total).is_err());
        assert!(after.adapter_and_framing > 0);
    }
}
