#[cfg(any(test, feature = "test-support"))]
pub mod mock;
pub mod openrouter;
pub mod types;

#[cfg(any(test, feature = "test-support"))]
pub use mock::MockLlmProvider;
pub use openrouter::OpenRouterProvider;
pub use types::*;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tauqe_protocol::ProviderKind;

use crate::config::AppConfig;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    async fn stream_chat(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()>;

    /// Effort-aware chat streaming.
    ///
    /// Providers that expose a native reasoning parameter translate the
    /// optional reasoning effort into it. The default implementation ignores
    /// the effort and delegates to `stream_chat`, so existing providers stay
    /// compatible without any change.
    async fn stream_chat_with_effort(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        _reasoning_effort: Option<ReasoningEffort>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.stream_chat(model, messages, response_format, tx, cancel_rx)
            .await
    }

    async fn stream_text(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.stream_chat(model, messages, None, tx, cancel_rx).await
    }
}

pub fn create_provider(kind: ProviderKind, config: &AppConfig) -> anyhow::Result<Box<dyn LlmProvider>> {
    match kind {
        ProviderKind::OpenRouter => {
            let api_key = config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref())
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("OpenRouter API key is not configured"))?;
            Ok(Box::new(OpenRouterProvider::new(api_key.to_string())))
        }
    }
}
