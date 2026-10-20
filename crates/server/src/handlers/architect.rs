//! Architect role subsystem.
//!
//! Provider resolution, tier rendering, streaming structured-chat turns, and the
//! multi-round discovery loop are shared by plan horizon synthesis and review
//! finding decomposition: both are the same Architect operation, so they run on
//! exactly one implementation here instead of a copy per subsystem.

mod discovery;
mod stream;

use std::sync::Arc;

use tauqe_core::config::AppConfig;
use tauqe_core::providers::{create_provider, LlmProvider};
use tauqe_protocol::{ArchitectTier, ModelRole};

use crate::state::AppState;

pub(crate) use discovery::{
    run_architect_discovery_loop, ArchitectDiscoveryError, ArchitectDiscoveryOutcome,
    ArchitectDiscoveryRequest,
};

/// Resolves the LLM provider for architect plan operations (refine and replan),
/// consulting the test override in test configurations before falling back to
/// network provider construction.
pub(crate) async fn resolve_architect_provider(
    _state: &Arc<AppState>,
    cfg: &AppConfig,
    provider_kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, String> {
    #[cfg(test)]
    {
        if let Some(p) = _state.provider_override.lock().await.clone() {
            return Ok(p);
        }
    }
    create_provider(provider_kind, cfg)
        .map(Arc::from)
        .map_err(|err| err.to_string())
}

/// Renders an Architect configuration for user-facing banners and logs.
pub(crate) fn format_architect_tier(tier: &ArchitectTier) -> String {
    match tier.effort {
        Some(effort) => format!("{} ({})", tier.model, effort),
        None => tier.model.to_string(),
    }
}

/// Alias of the configured Architect entry backing a resolved rung, if any.
///
/// The alias is looked up by the rung's model and reasoning depth, so an
/// escalated wave shows the label of the configuration it actually runs on
/// instead of the base entry's label.
pub(crate) fn architect_alias(cfg: &AppConfig, tier: &ArchitectTier) -> Option<String> {
    cfg.models
        .role_entries(ModelRole::Architect)
        .into_iter()
        .find(|entry| entry.model == tier.model && entry.effort == tier.effort)
        .and_then(|entry| entry.alias)
}
