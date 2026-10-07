pub mod openrouter;
pub mod types;

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
