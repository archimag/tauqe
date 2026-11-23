#[cfg(any(test, feature = "test-support"))]
pub mod mock;
mod openai;
pub use openai::login::{ChatGptSignIn, ChatGptSignInUi};
pub mod openrouter;
pub mod retry;
mod sse;
pub mod types;

#[cfg(any(test, feature = "test-support"))]
pub use mock::MockLlmProvider;
pub use openrouter::OpenRouterProvider;
pub use retry::{RetryConfig, RetryingProvider};
pub use types::*;

use crate::models::ProviderKind;
use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use crate::config::AppConfig;

/// Maximum accepted length of a session identifier on the wire.
const SESSION_ID_MAX_LEN: usize = 256;

/// Interactive REPL dialogue: one session per client session.
pub const SESSION_PREFIX_REPL: &str = "tauqe-repl";
/// Plan execution: one session for every step and retry of a plan.
pub const SESSION_PREFIX_PLAN: &str = "tauqe-plan";
/// Architect plan horizon synthesis.
pub const SESSION_PREFIX_ARCHITECT: &str = "tauqe-architect";
/// Review audit: findings, refinement, and group decomposition.
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

/// A single model invocation, independent of the provider's HTTP representation.
#[derive(Debug, Clone)]
pub struct LlmRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) response_format: Option<ResponseFormat>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) session_id: Option<&'a str>,
}

impl<'a> LlmRequest<'a> {
    #[cfg(test)]
    pub(crate) fn new(model: &'a str, messages: Vec<ChatMessage>) -> Self {
        Self {
            model,
            messages,
            response_format: None,
            reasoning_effort: None,
            session_id: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Sign in to {0} to continue")]
    AuthenticationRequired(ProviderKind),
    #[error("{0}")]
    PlanLimitReached(String),
    #[error(transparent)]
    Transient(anyhow::Error),
    #[error(transparent)]
    Request(anyhow::Error),
}

impl From<std::io::Error> for ProviderError {
    fn from(error: std::io::Error) -> Self {
        Self::Request(error.into())
    }
}

pub type ProviderResult = Result<(), ProviderError>;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    async fn stream(
        &self,
        request: LlmRequest<'_>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> ProviderResult;
}

fn chatgpt_credentials_path(config: &AppConfig) -> anyhow::Result<std::path::PathBuf> {
    config
        .explicit_credentials_path
        .clone()
        .or_else(|| {
            crate::config::paths::dirs_config()
                .or_else(|| crate::config::paths::dirs_home().map(|home| home.join(".config")))
                .map(|dir| dir.join("tauqe/credentials.toml"))
        })
        .ok_or_else(|| anyhow::anyhow!("Cannot locate user credentials"))
}

pub async fn sign_out_chatgpt(config: &AppConfig) -> anyhow::Result<bool> {
    openai::login::sign_out(chatgpt_credentials_path(config)?).await
}

/// Builds the provider for `kind`, wrapped in the universal retry decorator.
///
/// Every provider reaches the harness through [`RetryingProvider`], so a
/// transient transport failure — a dropped connection, an upstream `5xx`, a
/// broken event stream — is retried uniformly without any caller knowing about
/// the policy. Fatal failures, such as a rejected API key, are reported
/// immediately.
pub fn create_provider_with_login(
    kind: ProviderKind,
    config: &AppConfig,
    login: Option<std::sync::Arc<ChatGptSignIn>>,
) -> anyhow::Result<Box<dyn LlmProvider>> {
    let provider: Box<dyn LlmProvider> = match kind {
        ProviderKind::OpenAI => {
            let path = chatgpt_credentials_path(config)?;
            Box::new(openai::OpenAiProvider::new(path, login)?)
        }
        ProviderKind::OpenRouter => {
            let api_key = config
                .providers
                .openrouter
                .as_ref()
                .and_then(|o| o.api_key.as_deref())
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("OpenRouter API key is not configured"))?;
            Box::new(OpenRouterProvider::new(api_key.to_string()))
        }
    };
    Ok(Box::new(RetryingProvider::new(
        provider,
        RetryConfig::default(),
    )))
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
