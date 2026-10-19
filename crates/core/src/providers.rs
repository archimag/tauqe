#[cfg(any(test, feature = "test-support"))]
pub mod mock;
pub mod openrouter;
pub mod retry;
pub mod types;

#[cfg(any(test, feature = "test-support"))]
pub use mock::MockLlmProvider;
pub use openrouter::OpenRouterProvider;
pub use retry::{RetryConfig, RetryingProvider};
pub use types::*;

use async_trait::async_trait;
use tauqe_protocol::ProviderKind;
use tokio::sync::{mpsc, watch};

use crate::config::AppConfig;

/// Maximum accepted length of a session identifier on the wire.
pub const SESSION_ID_MAX_LEN: usize = 256;

/// Interactive REPL dialogue: one session per client session.
pub const SESSION_PREFIX_REPL: &str = "tauqe-repl";
/// Plan execution: one session for every step and retry of a plan.
pub const SESSION_PREFIX_PLAN: &str = "tauqe-plan";
/// Architect plan horizon synthesis.
pub const SESSION_PREFIX_ARCHITECT: &str = "tauqe-architect";
/// Review audit: findings, refinement, and subplan decomposition.
pub const SESSION_PREFIX_REVIEW: &str = "tauqe-review";

/// Joins a domain prefix with a domain identifier into a wire session id.
///
/// The result is truncated to [`SESSION_ID_MAX_LEN`] characters on a Unicode
/// scalar boundary, so an oversized identifier can neither produce an invalid
/// request nor panic on a multi-byte character.
pub fn format_session_id(prefix: &str, identifier: &str) -> String {
    let mut session = String::with_capacity(prefix.len() + identifier.len() + 1);
    if !prefix.is_empty() {
        session.push_str(prefix);
        session.push('-');
    }
    session.push_str(identifier);
    if session.chars().count() <= SESSION_ID_MAX_LEN {
        return session;
    }
    session.chars().take(SESSION_ID_MAX_LEN).collect()
}

// Provider callbacks carry the stream sender and the cancellation receiver on
// top of the payload, so a session-aware call reaches eight parameters.
#[allow(clippy::too_many_arguments)]
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

    /// Session-aware chat streaming.
    ///
    /// `session_id` binds one logical context (an interactive dialogue, a plan
    /// execution, a review audit) to a single downstream provider backend, so
    /// the backend keeps its prompt-prefix cache warm across requests.
    /// Providers that cannot express the affinity ignore the identifier; the
    /// default implementation forwards to `stream_chat_with_effort`.
    async fn stream_chat_with_session(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
        _session_id: Option<&str>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.stream_chat_with_effort(
            model,
            messages,
            response_format,
            reasoning_effort,
            tx,
            cancel_rx,
        )
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

/// Builds the provider for `kind`, wrapped in the universal retry decorator.
///
/// Every provider reaches the harness through [`RetryingProvider`], so a
/// transient transport failure — a dropped connection, an upstream `5xx`, a
/// broken event stream — is retried uniformly without any caller knowing about
/// the policy. Fatal failures, such as a rejected API key, are reported
/// immediately.
pub fn create_provider(
    kind: ProviderKind,
    config: &AppConfig,
) -> anyhow::Result<Box<dyn LlmProvider>> {
    match kind {
        ProviderKind::OpenRouter => {
            let api_key = config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref())
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("OpenRouter API key is not configured"))?;
            let provider = OpenRouterProvider::new(api_key.to_string());
            Ok(Box::new(RetryingProvider::new(
                Box::new(provider),
                RetryConfig::default(),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_session_id_joins_prefix_and_identifier() {
        assert_eq!(
            format_session_id(SESSION_PREFIX_PLAN, "p-42"),
            "tauqe-plan-p-42"
        );
    }

    #[test]
    fn format_session_id_omits_separator_for_empty_prefix() {
        assert_eq!(format_session_id("", "p-42"), "p-42");
    }

    #[test]
    fn format_session_id_truncates_long_identifiers_on_char_boundary() {
        let identifier = "п".repeat(SESSION_ID_MAX_LEN + 64);
        let session = format_session_id(SESSION_PREFIX_REVIEW, &identifier);
        assert_eq!(session.chars().count(), SESSION_ID_MAX_LEN);
        assert!(session.starts_with("tauqe-review-"));
    }
}
