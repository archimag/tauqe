//! OpenRouter wire-contract types.
//!
//! Request payloads, streaming chunks, usage DTOs, and error models exchanged
//! with the OpenRouter chat-completions and generation endpoints.

use serde::{Deserialize, Serialize};

use super::super::types::{ChatMessage, ReasoningEffort, ResponseFormat};

#[derive(Debug, Serialize)]
pub(crate) struct ChatCompletionRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<ReasoningOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<ProviderPreferences>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderPreferences {
    pub(crate) require_parameters: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct StreamOptions {
    pub(crate) include_usage: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReasoningOption {
    pub(crate) enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<ReasoningEffort>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamingChunk {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) choices: Vec<StreamingChoice>,
    #[serde(default)]
    pub(crate) usage: Option<OpenRouterUsage>,
    #[serde(default)]
    pub(crate) cost: Option<f64>,
    #[serde(default)]
    pub(crate) total_cost: Option<f64>,
    #[serde(default)]
    pub(crate) error: Option<OpenRouterError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GenerationResponse {
    pub(crate) data: Option<GenerationData>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GenerationData {
    #[serde(default)]
    pub(crate) total_cost: Option<f64>,
    #[serde(default)]
    pub(crate) tokens_prompt: Option<u32>,
    #[serde(default)]
    pub(crate) tokens_completion: Option<u32>,
    #[serde(default)]
    pub(crate) native_tokens_prompt: Option<u32>,
    #[serde(default)]
    pub(crate) native_tokens_completion: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct GenerationStats {
    pub total_cost: Option<f64>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamingChoice {
    #[serde(default)]
    pub(crate) delta: Option<StreamingDelta>,
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
    #[serde(default)]
    pub(crate) error: Option<OpenRouterError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamingDelta {
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) reasoning: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct OpenRouterUsage {
    #[serde(default)]
    pub(crate) prompt_tokens: u32,
    #[serde(default)]
    pub(crate) completion_tokens: u32,
    #[serde(default)]
    pub(crate) total_tokens: u32,
    #[serde(default)]
    pub(crate) cost: Option<f64>,
    #[serde(default)]
    pub(crate) total_cost: Option<f64>,
    #[serde(default)]
    pub(crate) prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub(crate) completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    pub(crate) cached_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct CompletionTokensDetails {
    #[serde(default)]
    pub(crate) reasoning_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenRouterError {
    pub(crate) message: String,
}
